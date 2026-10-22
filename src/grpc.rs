use futures::channel::{mpsc::UnboundedSender, oneshot};
use prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, Kind, MessageDescriptor, MethodDescriptor, SerializeOptions,
    Value,
};
use prost_types::{FileDescriptorProto, FileDescriptorSet};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use http::uri::PathAndQuery;
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};

type Client = tonic::client::Grpc<Channel>;

pub enum Source {
    Reflection,
    /// A .proto file; its parent directory is the import root.
    Proto(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MethodInfo {
    /// "pkg.Service/Method"
    pub path: String,
    pub client_streaming: bool,
    pub server_streaming: bool,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// Response headers (initial metadata).
    Metadata(Vec<(String, String)>),
    /// One response message as pretty JSON.
    Message(String),
    /// `status` is code number and name, e.g. "0 OK" or "5 NOT_FOUND".
    Done { status: String, message: String, trailers: Vec<(String, String)> },
    /// Connection, descriptor or encoding failure; nothing follows it.
    Failed(String),
}

/// Every method of every service, sorted. Blocking; call it off the UI thread.
pub fn list_methods(
    url: &str,
    source: &Source,
    metadata: &[(String, String)],
) -> Result<Vec<MethodInfo>, String> {
    let pool = block_on(load_pool(url, source, metadata))?;
    let mut methods: Vec<MethodInfo> = pool
        .services()
        .filter(|s| !s.full_name().starts_with("grpc.reflection."))
        .flat_map(|s| s.methods().collect::<Vec<_>>())
        .map(|m| MethodInfo {
            path: method_path(&m),
            client_streaming: m.is_client_streaming(),
            server_streaming: m.is_server_streaming(),
        })
        .collect();
    methods.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(methods)
}

/// Pretty JSON for the method's input with every field at its default. Blocking.
pub fn example_message(
    url: &str,
    source: &Source,
    metadata: &[(String, String)],
    method: &str,
) -> Result<String, String> {
    let pool = block_on(load_pool(url, source, metadata))?;
    to_json(&template(find_method(&pool, method)?.input(), &mut Vec::new()))
}

/// Sends `message_json` as the only request message, then half-closes, so it serves every
/// streaming kind. Blocking; returns without further events once `cancel` resolves or is dropped.
pub fn invoke(
    url: &str,
    source: &Source,
    metadata: &[(String, String)],
    method: &str,
    message_json: &str,
    cancel: oneshot::Receiver<()>,
    events: UnboundedSender<Event>,
) {
    block_on(async {
        let call = std::pin::pin!(call(url, source, metadata, method, message_json, &events));
        if let futures::future::Either::Left((Err(e), _)) = futures::future::select(call, cancel).await {
            let _ = events.unbounded_send(Event::Failed(e));
        }
    })
}

async fn call(
    url: &str,
    source: &Source,
    metadata: &[(String, String)],
    method: &str,
    message_json: &str,
    events: &UnboundedSender<Event>,
) -> Result<(), String> {
    let send = |event| {
        let _ = events.unbounded_send(event);
    };
    let metadata = metadata_map(metadata)?;
    let mut client = connect(url).await?;
    let pool = match source {
        Source::Proto(path) => compile_proto(path)?,
        Source::Reflection => reflect(&mut client, &metadata).await?,
    };
    let method = find_method(&pool, method)?;
    let mut json = serde_json::Deserializer::from_str(message_json);
    let message = DynamicMessage::deserialize(method.input(), &mut json)
        .and_then(|m| json.end().map(|_| m))
        .map_err(|e| format!("Invalid message JSON: {e}"))?;
    let path = PathAndQuery::try_from(format!("/{}", method_path(&method))).map_err(|e| e.to_string())?;
    let mut request = Request::new(futures::stream::iter([message]));
    *request.metadata_mut() = metadata;
    client.ready().await.map_err(|e| describe(&e))?;
    let done = |status: Status| Event::Done {
        status: code_name(status.code()),
        message: status.message().into(),
        trailers: pairs(status.metadata()),
    };
    let response = match client.streaming(request, path, DynCodec(method.output())).await {
        Ok(response) => response,
        Err(status) => return Ok(send(done(status))),
    };
    send(Event::Metadata(pairs(response.metadata())));
    let mut stream = response.into_inner();
    loop {
        match stream.message().await {
            Ok(Some(message)) => send(Event::Message(to_json(&message)?)),
            Ok(None) => break,
            Err(status) => return Ok(send(done(status))),
        }
    }
    let trailers = stream.trailers().await.ok().flatten().unwrap_or_default();
    send(Event::Done { status: code_name(Code::Ok), message: String::new(), trailers: pairs(&trailers) });
    Ok(())
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

async fn load_pool(url: &str, source: &Source, metadata: &[(String, String)]) -> Result<DescriptorPool, String> {
    match source {
        Source::Proto(path) => compile_proto(path),
        Source::Reflection => {
            let metadata = metadata_map(metadata)?;
            reflect(&mut connect(url).await?, &metadata).await
        }
    }
}

async fn connect(url: &str) -> Result<Client, String> {
    let url = url.trim();
    let (tls, rest) = match url.split_once("://") {
        Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
            "grpcs" | "https" => (true, rest),
            "grpc" | "http" => (false, rest),
            other => return Err(format!("Unsupported URL scheme \"{other}\"")),
        },
        None => (false, url),
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err("Enter a server URL".into());
    }
    // "[::1]" has colons but no port; "[::1]:50051" does.
    let has_port = authority.rsplit_once(':').is_some_and(|(_, port)| !port.contains(']'));
    let port = if has_port { "" } else if tls { ":443" } else { ":80" };
    let scheme = if tls { "https" } else { "http" };
    let mut endpoint = Endpoint::from_shared(format!("{scheme}://{authority}{port}"))
        .map_err(|e| describe(&e))?
        // ponytail: fixed 10s connect timeout, expose it if users ask.
        .connect_timeout(Duration::from_secs(10));
    if tls {
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().with_webpki_roots())
            .map_err(|e| describe(&e))?;
    }
    let channel = endpoint.connect().await.map_err(|e| describe(&e))?;
    Ok(tonic::client::Grpc::new(channel))
}

// ponytail: ASCII metadata only; "-bin" keys are rejected as invalid.
fn metadata_map(pairs: &[(String, String)]) -> Result<MetadataMap, String> {
    let mut map = MetadataMap::new();
    for (key, value) in pairs {
        let name = MetadataKey::from_bytes(key.trim().to_ascii_lowercase().as_bytes())
            .map_err(|_| format!("Invalid metadata key \"{key}\""))?;
        let value = MetadataValue::try_from(value.as_str())
            .map_err(|_| format!("Invalid value for metadata \"{key}\""))?;
        map.append(name, value);
    }
    Ok(map)
}

fn compile_proto(path: &Path) -> Result<DescriptorPool, String> {
    let root = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut compiler = protox::Compiler::new([root]).map_err(|e| e.to_string())?;
    compiler.include_imports(true).open_file(path).map_err(|e| e.to_string())?;
    Ok(compiler.descriptor_pool())
}

fn find_method(pool: &DescriptorPool, method: &str) -> Result<MethodDescriptor, String> {
    method
        .trim()
        .trim_start_matches('/')
        .split_once('/')
        .and_then(|(service, name)| pool.get_service_by_name(service)?.methods().find(|m| m.name() == name))
        .ok_or_else(|| format!("Method \"{method}\" not found"))
}

fn method_path(method: &MethodDescriptor) -> String {
    format!("{}/{}", method.parent_service().full_name(), method.name())
}

/// Default message with singular sub-messages filled in, so the template shows nested fields.
fn template(desc: MessageDescriptor, seen: &mut Vec<String>) -> DynamicMessage {
    let mut message = DynamicMessage::new(desc.clone());
    seen.push(desc.full_name().into());
    for field in desc.fields() {
        let Kind::Message(inner) = field.kind() else { continue };
        // Well-known types have special JSON forms (Any needs a type URL), so leave them unset.
        let skip = field.is_list()
            || field.is_map()
            || field.containing_oneof().is_some()
            || inner.full_name().starts_with("google.protobuf.")
            || seen.iter().any(|s| s == inner.full_name());
        if !skip {
            message.set_field(&field, Value::Message(template(inner, seen)));
        }
    }
    seen.pop();
    message
}

fn to_json(message: &DynamicMessage) -> Result<String, String> {
    let mut out = Vec::new();
    let options = SerializeOptions::new().skip_default_fields(false);
    message
        .serialize_with_options(&mut serde_json::Serializer::pretty(&mut out), &options)
        .map_err(|e| format!("Could not encode message as JSON: {e}"))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn pairs(metadata: &MetadataMap) -> Vec<(String, String)> {
    metadata
        .clone()
        .into_headers()
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "grpc-status" | "grpc-message"))
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect()
}

/// "5 NOT_FOUND" from `Code::NotFound`.
fn code_name(code: Code) -> String {
    let mut name = String::new();
    for (i, c) in format!("{code:?}").chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            name.push('_');
        }
        name.push(c.to_ascii_uppercase());
    }
    format!("{} {name}", code as i32)
}

fn describe(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    message
}

struct DynCodec(MessageDescriptor);

impl Codec for DynCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynCodec;
    type Decoder = DynCodec;

    fn encoder(&mut self) -> DynCodec {
        DynCodec(self.0.clone())
    }

    fn decoder(&mut self) -> DynCodec {
        DynCodec(self.0.clone())
    }
}

impl Encoder for DynCodec {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: DynamicMessage, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(dst).map_err(|e| Status::internal(e.to_string()))
    }
}

impl Decoder for DynCodec {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<DynamicMessage>, Status> {
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|e| Status::internal(e.to_string()))
    }
}

// Reflection messages declare their oneof members as plain optional fields; the wire format is the same.
#[derive(Clone, PartialEq, prost::Message)]
struct ReflectionRequest {
    #[prost(string, optional, tag = "3")]
    file_by_filename: Option<String>,
    #[prost(string, optional, tag = "4")]
    file_containing_symbol: Option<String>,
    #[prost(string, optional, tag = "7")]
    list_services: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ReflectionResponse {
    #[prost(message, optional, tag = "4")]
    file_descriptor_response: Option<FileDescriptors>,
    #[prost(message, optional, tag = "6")]
    list_services_response: Option<ServiceList>,
    #[prost(message, optional, tag = "7")]
    error_response: Option<ReflectionError>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct FileDescriptors {
    #[prost(bytes = "vec", repeated, tag = "1")]
    file_descriptor_proto: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ServiceList {
    #[prost(message, repeated, tag = "1")]
    service: Vec<ServiceName>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ServiceName {
    #[prost(string, tag = "1")]
    name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ReflectionError {
    #[prost(int32, tag = "1")]
    error_code: i32,
    #[prost(string, tag = "2")]
    error_message: String,
}

const REFLECTION_V1ALPHA: &str = "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";
const REFLECTION_V1: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";

async fn reflect(client: &mut Client, metadata: &MetadataMap) -> Result<DescriptorPool, String> {
    let failed = |status: Status| format!("Server reflection failed: {} {}", code_name(status.code()), status.message());
    let list = ReflectionRequest { list_services: Some(String::new()), ..Default::default() };
    let mut path = REFLECTION_V1ALPHA;
    let services = match ask(client, path, metadata, list.clone()).await {
        Err(status) if status.code() == Code::Unimplemented => {
            path = REFLECTION_V1;
            ask(client, path, metadata, list).await
        }
        other => other,
    }
    .map_err(failed)?
    .list_services_response
    .ok_or("Server reflection returned no service list")?
    .service;

    let mut files = HashMap::new();
    for service in services.iter().filter(|s| !s.name.starts_with("grpc.reflection.")) {
        let request = ReflectionRequest { file_containing_symbol: Some(service.name.clone()), ..Default::default() };
        add_files(&mut files, ask(client, path, metadata, request).await.map_err(failed)?)?;
    }
    loop {
        let missing: BTreeSet<String> = files
            .values()
            .flat_map(|f: &FileDescriptorProto| f.dependency.iter())
            .filter(|d| !files.contains_key(*d))
            .cloned()
            .collect();
        if missing.is_empty() {
            break;
        }
        for name in missing {
            let request = ReflectionRequest { file_by_filename: Some(name.clone()), ..Default::default() };
            add_files(&mut files, ask(client, path, metadata, request).await.map_err(failed)?)?;
            if !files.contains_key(&name) {
                return Err(format!("Server reflection did not return \"{name}\""));
            }
        }
    }
    DescriptorPool::from_file_descriptor_set(FileDescriptorSet { file: files.into_values().collect() })
        .map_err(|e| format!("Invalid descriptors from server reflection: {e}"))
}

// ponytail: one reflection RPC per request; a single shared stream would save round trips.
async fn ask(
    client: &mut Client,
    path: &'static str,
    metadata: &MetadataMap,
    message: ReflectionRequest,
) -> Result<ReflectionResponse, Status> {
    let mut request = Request::new(message);
    *request.metadata_mut() = metadata.clone();
    client.ready().await.map_err(|e| Status::unavailable(describe(&e)))?;
    let codec = tonic_prost::ProstCodec::<ReflectionRequest, ReflectionResponse>::default();
    let mut stream = client
        .server_streaming(request, PathAndQuery::from_static(path), codec)
        .await?
        .into_inner();
    stream.message().await?.ok_or_else(|| Status::internal("empty reflection response"))
}

fn add_files(files: &mut HashMap<String, FileDescriptorProto>, response: ReflectionResponse) -> Result<(), String> {
    if let Some(error) = response.error_response {
        return Err(format!("Server reflection: {}", error.error_message));
    }
    for bytes in response.file_descriptor_response.map(|f| f.file_descriptor_proto).unwrap_or_default() {
        let file = FileDescriptorProto::decode(bytes.as_slice()).map_err(|e| format!("Invalid descriptor from server reflection: {e}"))?;
        files.insert(file.name().to_string(), file);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use futures::future::{ready, BoxFuture, Ready};
    use futures::StreamExt;
    use std::convert::Infallible;
    use std::sync::OnceLock;
    use std::task::{Context, Poll};
    use tonic::body::Body;
    use tonic::codegen::Service;
    use tonic::server::{NamedService, ServerStreamingService, UnaryService};
    use tonic::transport::server::TcpIncoming;
    use tonic::Response;

    const PROTO: &str = r#"syntax = "proto3";
package test;
message Inner { string note = 1; }
message Msg { string text = 1; int32 count = 2; string token = 3; Inner inner = 4; }
service Echo {
  rpc Say(Msg) returns (Msg);
  rpc Repeat(Msg) returns (stream Msg);
}
"#;

    /// Serves test.Echo plus v1-only reflection, so the v1alpha → v1 fallback is exercised.
    fn server() -> &'static (String, PathBuf) {
        static SERVER: OnceLock<(String, PathBuf)> = OnceLock::new();
        SERVER.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("floki-grpc-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let proto = dir.join("echo.proto");
            std::fs::write(&proto, PROTO).unwrap();
            let descriptors = protox::compile([&proto], [&dir]).unwrap();
            let pool = DescriptorPool::from_file_descriptor_set(descriptors.clone()).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                block_on(async move {
                    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                    tx.send(incoming.local_addr().unwrap()).unwrap();
                    let reflection = tonic_reflection::server::Builder::configure()
                        .register_file_descriptor_set(descriptors)
                        .build_v1()
                        .unwrap();
                    tonic::transport::Server::builder()
                        .add_service(Echo(pool))
                        .add_service(reflection)
                        .serve_with_incoming(incoming)
                        .await
                        .unwrap();
                })
            });
            (format!("grpc://{}", rx.recv().unwrap()), proto)
        })
    }

    #[derive(Clone)]
    struct Echo(DescriptorPool);

    impl NamedService for Echo {
        const NAME: &'static str = "test.Echo";
    }

    impl Service<http::Request<Body>> for Echo {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<Body>) -> Self::Future {
            let mut grpc = tonic::server::Grpc::new(DynCodec(self.0.get_message_by_name("test.Msg").unwrap()));
            Box::pin(async move {
                Ok(if request.uri().path().ends_with("/Say") {
                    grpc.unary(Handler, request).await
                } else {
                    grpc.server_streaming(Handler, request).await
                })
            })
        }
    }

    struct Handler;

    /// Echoes the request with `token` set from the x-token metadata.
    fn reply(request: Request<DynamicMessage>) -> DynamicMessage {
        let token = request.metadata().get("x-token").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let mut message = request.into_inner();
        message.set_field_by_name("token", Value::String(token));
        message
    }

    impl UnaryService<DynamicMessage> for Handler {
        type Response = DynamicMessage;
        type Future = Ready<Result<Response<DynamicMessage>, Status>>;

        fn call(&mut self, request: Request<DynamicMessage>) -> Self::Future {
            ready(Ok(Response::new(reply(request))))
        }
    }

    impl ServerStreamingService<DynamicMessage> for Handler {
        type Response = DynamicMessage;
        type ResponseStream = futures::stream::Iter<std::vec::IntoIter<Result<DynamicMessage, Status>>>;
        type Future = Ready<Result<Response<Self::ResponseStream>, Status>>;

        fn call(&mut self, request: Request<DynamicMessage>) -> Self::Future {
            let message = reply(request);
            let count = message.get_field_by_name("count").and_then(|v| v.as_i32()).unwrap_or(0);
            let items: Vec<_> = (0..count).map(|_| Ok(message.clone())).collect();
            ready(Ok(Response::new(futures::stream::iter(items))))
        }
    }

    fn proto() -> Source {
        Source::Proto(server().1.clone())
    }

    fn run(source: Source, method: &str, json: &str) -> Vec<Event> {
        let (tx, rx) = mpsc::unbounded();
        let (_cancel, cancel) = oneshot::channel();
        let metadata = [("X-Token".to_string(), "secret".to_string())];
        invoke(&server().0, &source, &metadata, method, json, cancel, tx);
        futures::executor::block_on(rx.collect())
    }

    fn json(event: &Event) -> serde_json::Value {
        match event {
            Event::Message(text) => serde_json::from_str(text).unwrap(),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn lists_methods_from_proto_and_reflection() {
        let expected = vec![
            MethodInfo { path: "test.Echo/Repeat".into(), client_streaming: false, server_streaming: true },
            MethodInfo { path: "test.Echo/Say".into(), client_streaming: false, server_streaming: false },
        ];
        assert_eq!(list_methods("", &proto(), &[]).unwrap(), expected);
        assert_eq!(list_methods(&server().0, &Source::Reflection, &[]).unwrap(), expected);
    }

    #[test]
    fn example_has_every_field() {
        let example = example_message(&server().0, &Source::Reflection, &[], "test.Echo/Say").unwrap();
        let value: serde_json::Value = serde_json::from_str(&example).unwrap();
        assert_eq!(value, serde_json::json!({"text": "", "count": 0, "token": "", "inner": {"note": ""}}));
    }

    #[test]
    fn unary_call_echoes_message_and_metadata() {
        let events = run(proto(), "test.Echo/Say", r#"{"text": "hi"}"#);
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(&events[0], Event::Metadata(_)));
        let reply = json(&events[1]);
        assert_eq!((reply["text"].as_str(), reply["token"].as_str()), (Some("hi"), Some("secret")));
        assert!(matches!(&events[2], Event::Done { status, .. } if status == "0 OK"));
    }

    #[test]
    fn server_streaming_emits_each_message() {
        let events = run(Source::Reflection, "test.Echo/Repeat", r#"{"text": "x", "count": 3}"#);
        let messages = events.iter().filter(|e| matches!(e, Event::Message(_))).count();
        assert_eq!(messages, 3, "{events:?}");
        assert!(matches!(events.last(), Some(Event::Done { status, .. }) if status == "0 OK"));
    }

    #[test]
    fn bad_input_fails() {
        assert_eq!(run(proto(), "test.Echo/Nope", "{}"), [Event::Failed("Method \"test.Echo/Nope\" not found".into())]);
        let events = run(proto(), "test.Echo/Say", "{\"text\": 1");
        assert!(matches!(&events[..], [Event::Failed(e)] if e.starts_with("Invalid message JSON")), "{events:?}");
        let bad_key = list_methods(&server().0, &Source::Reflection, &[("bad key".into(), "v".into())]);
        assert_eq!(bad_key, Err("Invalid metadata key \"bad key\"".into()));
    }

    #[test]
    fn code_names() {
        assert_eq!(code_name(Code::Ok), "0 OK");
        assert_eq!(code_name(Code::NotFound), "5 NOT_FOUND");
        assert_eq!(code_name(Code::DeadlineExceeded), "4 DEADLINE_EXCEEDED");
    }
}
