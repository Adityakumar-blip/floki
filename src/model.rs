use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
pub const WEBSOCKET: &str = "WS";
pub const SOCKET_IO: &str = "SOCKETIO";
pub const GRPC: &str = "GRPC";

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// form-data only: `value` is a file path.
    #[serde(default, skip_serializing_if = "is_false")]
    pub file: bool,
}

impl KeyValue {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            enabled: true,
            file: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Auth {
    #[default]
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    ApiKey {
        key: String,
        value: String,
        in_query: bool,
    },
    Digest {
        username: String,
        password: String,
    },
    #[serde(rename = "oauth2")]
    OAuth2(OAuth2),
    #[serde(rename = "awsv4")]
    AwsV4 {
        access_key: String,
        secret_key: String,
        region: String,
        service: String,
        session_token: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OAuth2 {
    pub access_token: String,
    pub header_prefix: String,
    /// `client_credentials` or `password`; used by "Get New Access Token".
    pub grant_type: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    pub username: String,
    pub password: String,
}

impl Default for OAuth2 {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            header_prefix: "Bearer".into(),
            grant_type: "client_credentials".into(),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: String::new(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl Auth {
    // ponytail: no NTLM, OAuth 1.0, Hawk, JWT, Akamai or ASAP; add when someone needs them.
    pub const LABELS: &[&str] =
        &["No Auth", "Basic Auth", "Bearer Token", "Digest Auth", "OAuth 2.0", "AWS Signature", "API Key"];

    pub fn label(&self) -> &'static str {
        Self::LABELS[self.index()]
    }

    pub fn index(&self) -> usize {
        match self {
            Auth::None => 0,
            Auth::Basic { .. } => 1,
            Auth::Bearer { .. } => 2,
            Auth::Digest { .. } => 3,
            Auth::OAuth2(_) => 4,
            Auth::AwsV4 { .. } => 5,
            Auth::ApiKey { .. } => 6,
        }
    }

    pub fn from_index(index: usize) -> Self {
        let s = String::new;
        match index {
            1 => Auth::Basic { username: s(), password: s() },
            2 => Auth::Bearer { token: s() },
            3 => Auth::Digest { username: s(), password: s() },
            4 => Auth::OAuth2(OAuth2::default()),
            5 => Auth::AwsV4 {
                access_key: s(),
                secret_key: s(),
                region: "us-east-1".into(),
                service: s(),
                session_token: s(),
            },
            6 => Auth::ApiKey { key: s(), value: s(), in_query: false },
            _ => Auth::None,
        }
    }

    /// The editable text fields, labelled, in display order.
    pub fn fields_mut(&mut self) -> Vec<(&'static str, &mut String)> {
        match self {
            Auth::None => vec![],
            Auth::Bearer { token } => vec![("Token", token)],
            Auth::Basic { username, password } | Auth::Digest { username, password } => {
                vec![("Username", username), ("Password", password)]
            }
            Auth::ApiKey { key, value, .. } => vec![("Key", key), ("Value", value)],
            Auth::OAuth2(o) => {
                let mut fields = vec![
                    ("Access Token", &mut o.access_token),
                    ("Header Prefix", &mut o.header_prefix),
                    ("Access Token URL", &mut o.token_url),
                    ("Client ID", &mut o.client_id),
                    ("Client Secret", &mut o.client_secret),
                    ("Scope", &mut o.scope),
                ];
                if o.grant_type == "password" {
                    fields.push(("Username", &mut o.username));
                    fields.push(("Password", &mut o.password));
                }
                fields
            }
            Auth::AwsV4 { access_key, secret_key, region, service, session_token } => vec![
                ("AccessKey", access_key),
                ("SecretKey", secret_key),
                ("AWS Region", region),
                ("Service Name", service),
                ("Session Token", session_token),
            ],
        }
    }

    pub fn fields(&self) -> Vec<(&'static str, String)> {
        self.clone().fields_mut().into_iter().map(|(k, v)| (k, v.clone())).collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyMode {
    #[default]
    None,
    FormData,
    UrlEncoded,
    Raw,
    #[serde(rename = "graphql")]
    GraphQl,
    /// Sends the file at `Request::body_file` as the whole body.
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryEncoding {
    Base64,
    Hex,
}

impl BinaryEncoding {
    pub fn decode(self, text: &str) -> Result<Vec<u8>, String> {
        use base64::Engine;
        let compact: String = text.split_whitespace().collect();
        match self {
            BinaryEncoding::Base64 => base64::engine::general_purpose::STANDARD
                .decode(compact)
                .map_err(|e| format!("Invalid Base64: {e}")),
            BinaryEncoding::Hex => {
                let compact = compact.trim_start_matches("0x");
                if compact.len() % 2 != 0 {
                    return Err("Invalid hex: odd number of digits".into());
                }
                (0..compact.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).map_err(|_| "Invalid hex digit".to_string()))
                    .collect()
            }
        }
    }
}

/// Connection settings shared by WebSocket and Socket.IO requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SocketSettings {
    /// 0 waits forever.
    pub handshake_timeout_ms: u64,
    pub reconnect_attempts: u32,
    pub reconnect_interval_ms: u64,
    pub max_message_mb: u32,
    /// Socket.IO only; the URL path names the namespace.
    pub handshake_path: String,
}

impl Default for SocketSettings {
    fn default() -> Self {
        Self {
            handshake_timeout_ms: 0,
            reconnect_attempts: 0,
            reconnect_interval_ms: 5000,
            max_message_mb: 10,
            handshake_path: "/socket.io".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawLanguage {
    Text,
    JavaScript,
    #[default]
    Json,
    Html,
    Xml,
}

impl RawLanguage {
    pub const ALL: &[RawLanguage] = &[
        RawLanguage::Text,
        RawLanguage::JavaScript,
        RawLanguage::Json,
        RawLanguage::Html,
        RawLanguage::Xml,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RawLanguage::Text => "Text",
            RawLanguage::JavaScript => "JavaScript",
            RawLanguage::Json => "JSON",
            RawLanguage::Html => "HTML",
            RawLanguage::Xml => "XML",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            RawLanguage::Text => "text/plain",
            RawLanguage::JavaScript => "application/javascript",
            RawLanguage::Json => "application/json",
            RawLanguage::Html => "text/html",
            RawLanguage::Xml => "application/xml",
        }
    }

    /// Name understood by the code editor's highlighter.
    pub fn editor_language(self) -> &'static str {
        match self {
            RawLanguage::Text | RawLanguage::Xml => "text",
            RawLanguage::JavaScript => "javascript",
            RawLanguage::Json => "json",
            RawLanguage::Html => "html",
        }
    }

    fn postman_name(self) -> &'static str {
        match self {
            RawLanguage::Text => "text",
            RawLanguage::JavaScript => "javascript",
            RawLanguage::Json => "json",
            RawLanguage::Html => "html",
            RawLanguage::Xml => "xml",
        }
    }

    fn from_postman(name: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|lang| lang.postman_name() == name)
            .unwrap_or(RawLanguage::Text)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Request {
    pub name: String,
    pub method: String,
    pub url: String,
    /// Source of truth for the query string; enabled rows are mirrored into `url`.
    pub params: Vec<KeyValue>,
    pub headers: Vec<KeyValue>,
    pub auth: Auth,
    pub body_mode: BodyMode,
    pub raw_language: RawLanguage,
    pub body_raw: String,
    pub body_form: Vec<KeyValue>,
    pub body_multipart: Vec<KeyValue>,
    pub graphql_query: String,
    pub graphql_variables: String,
    pub body_file: String,
    pub follow_redirects: bool,
    pub verify_ssl: bool,
    /// Socket message composer: `Some` sends `body_raw` decoded as binary.
    pub message_binary: Option<BinaryEncoding>,
    pub socket: SocketSettings,
    pub socket_event: String,
    pub socket_ack: bool,
    /// gRPC "package.Service/Method".
    pub grpc_method: String,
    /// gRPC .proto file; empty uses server reflection.
    pub grpc_proto: String,
    /// Request description shown in the Docs tab.
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<Example>,
}

/// A saved response shown under its request.
// ponytail: no per-example original request; add when examples need their own params/body.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Example {
    pub name: String,
    pub status: u16,
    pub headers: Vec<KeyValue>,
    pub body: String,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            name: String::new(),
            method: "GET".into(),
            url: String::new(),
            params: Vec::new(),
            headers: Vec::new(),
            auth: Auth::None,
            body_mode: BodyMode::None,
            raw_language: RawLanguage::Json,
            body_raw: String::new(),
            body_form: Vec::new(),
            body_multipart: Vec::new(),
            graphql_query: String::new(),
            graphql_variables: String::new(),
            body_file: String::new(),
            follow_redirects: true,
            verify_ssl: true,
            message_binary: None,
            socket: SocketSettings::default(),
            socket_event: "message".into(),
            socket_ack: false,
            grpc_method: String::new(),
            grpc_proto: String::new(),
            description: String::new(),
            examples: Vec::new(),
        }
    }
}

impl Request {
    /// Tab and sidebar title: saved name, else the URL.
    pub fn title(&self) -> String {
        if !self.name.trim().is_empty() {
            self.name.clone()
        } else if !self.url.trim().is_empty() {
            self.url.clone()
        } else {
            "Untitled Request".into()
        }
    }

    /// Non-HTTP requests are stored with a pseudo-method: `WS`, `SOCKETIO` or `GRPC`.
    pub fn is_websocket(&self) -> bool {
        self.method == WEBSOCKET
    }

    pub fn is_socket_io(&self) -> bool {
        self.method == SOCKET_IO
    }

    /// WebSocket or Socket.IO: a long-lived connection with a message log.
    pub fn is_socket(&self) -> bool {
        self.is_websocket() || self.is_socket_io()
    }

    pub fn is_grpc(&self) -> bool {
        self.method == GRPC
    }

    /// Also switches the request kind when the URL's scheme says so; Socket.IO accepts http URLs.
    pub fn set_url(&mut self, url: String) {
        let scheme = url.trim_start().split_once("://").map(|(s, _)| s.to_ascii_lowercase());
        match scheme.as_deref() {
            Some("ws" | "wss") if !self.is_socket() => self.method = WEBSOCKET.into(),
            Some("grpc" | "grpcs") => self.method = GRPC.into(),
            Some("http" | "https") if self.is_websocket() || self.is_grpc() => self.method = "GET".into(),
            _ => {}
        }
        self.params = params_from_url(&url, &self.params);
        self.url = url;
    }

    pub fn set_params(&mut self, params: Vec<KeyValue>) {
        self.url = url_with_params(&self.url, &params);
        self.params = params;
    }
}

/// Splits `url` into (base, query, fragment) without decoding anything.
fn split_url(url: &str) -> (&str, Option<&str>, &str) {
    let (rest, fragment) = match url.find('#') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..]), fragment),
        None => (rest, None, fragment),
    }
}

/// Query rows parsed from the URL, keeping previously disabled rows.
pub fn params_from_url(url: &str, previous: &[KeyValue]) -> Vec<KeyValue> {
    let (_, query, _) = split_url(url);
    let mut params: Vec<KeyValue> = query
        .map(|q| {
            q.split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) => KeyValue::new(k, v),
                    None => KeyValue::new(pair, ""),
                })
                .collect()
        })
        .unwrap_or_default();
    params.extend(previous.iter().filter(|p| !p.enabled).cloned());
    params
}

pub fn url_with_params(url: &str, params: &[KeyValue]) -> String {
    let (base, _, fragment) = split_url(url);
    let query = params
        .iter()
        .filter(|p| p.enabled && !(p.key.is_empty() && p.value.is_empty()))
        .map(|p| {
            if p.value.is_empty() && !p.key.is_empty() {
                p.key.clone()
            } else {
                format!("{}={}", p.key, p.value)
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        format!("{base}{fragment}")
    } else {
        format!("{base}?{query}{fragment}")
    }
}

/// Replaces `{{name}}` with the last matching variable; unknown names stay as written.
pub fn resolve(source: &str, vars: &[(String, String)]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let Some(end) = tail.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = tail[..end].trim();
        match vars.iter().rev().find(|(key, _)| key == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &tail[end + 2..];
    }
    out.push_str(rest);
    out
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedRequest {
    pub id: u64,
    pub request: Request,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Folder {
    pub id: u64,
    pub name: String,
    pub items: Vec<Item>,
    #[serde(default)]
    pub expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    Folder(Folder),
    Request(SavedRequest),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub variables: Vec<KeyValue>,
    #[serde(default)]
    pub expanded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub values: Vec<KeyValue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub request: Request,
    pub status: Option<u16>,
    pub at_unix_ms: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub collections: Vec<Collection>,
    pub environments: Vec<Environment>,
    pub active_environment: Option<u64>,
    pub history: Vec<HistoryEntry>,
    pub theme: ThemeMode,
    pub next_id: u64,
    pub globals: Vec<KeyValue>,
    pub settings: Settings,
}

/// The parts of a workspace that are synced to the server; history, theme and settings stay on this machine.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedData {
    pub collections: Vec<Collection>,
    pub environments: Vec<Environment>,
    pub globals: Vec<KeyValue>,
    pub next_id: u64,
}

impl Workspace {
    pub fn shared(&self) -> SharedData {
        SharedData {
            collections: self.collections.clone(),
            environments: self.environments.clone(),
            globals: self.globals.clone(),
            next_id: self.next_id,
        }
    }

    pub fn apply_shared(&mut self, data: SharedData) {
        self.collections = data.collections;
        self.environments = data.environments;
        self.globals = data.globals;
        // ponytail: ids are allocated per client, so two people adding at once can collide; the sync conflict path catches it.
        self.next_id = self.next_id.max(data.next_id);
        if self.active_environment.is_some_and(|id| !self.environments.iter().any(|e| e.id == id)) {
            self.active_environment = None;
        }
    }
}

/// The workspace list and signed-in account (`app.json`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppData {
    pub workspaces: Vec<WorkspaceMeta>,
    pub active: String,
    pub account: Option<Account>,
}

impl AppData {
    pub fn new_local_id(&self) -> String {
        let max = self.workspaces.iter().filter_map(|w| w.id.parse::<u64>().ok()).max().unwrap_or(0);
        (max + 1).to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    /// Local file id.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub remote: Option<Remote>,
}

/// Link from a local workspace to its copy on the sync server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Remote {
    pub id: String,
    pub version: i64,
    /// `admin`, `editor` or `viewer`.
    pub role: String,
    pub team: Option<String>,
    /// Local changes not yet pushed.
    #[serde(default)]
    pub dirty: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub server: String,
    // ponytail: session token stored in plain text like the rest of the data file; use the OS keyring if this ships widely.
    pub token: String,
    pub email: String,
    pub name: String,
    /// Invites only reach accounts whose email address is confirmed.
    #[serde(default)]
    pub verified: bool,
}

/// App-wide HTTP settings (Settings > Proxy and certificates).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Empty uses the system proxy from the environment.
    pub proxy: String,
    /// Extra trusted CA certificate (PEM file).
    pub ca_cert: String,
    /// Client certificate and private key (PEM files), sent to every host.
    pub client_cert: String,
    pub client_key: String,
}

pub const HISTORY_LIMIT: usize = 500;
/// `alloc_id` starts at 1, so 0 can name the globals.
pub const GLOBALS_ID: u64 = 0;

fn find_in<'a>(items: &'a [Item], id: u64) -> Option<&'a SavedRequest> {
    items.iter().find_map(|item| match item {
        Item::Request(r) if r.id == id => Some(r),
        Item::Folder(f) => find_in(&f.items, id),
        _ => None,
    })
}

fn find_in_mut(items: &mut [Item], id: u64) -> Option<&mut SavedRequest> {
    items.iter_mut().find_map(|item| match item {
        Item::Request(r) if r.id == id => Some(r),
        Item::Folder(f) => find_in_mut(&mut f.items, id),
        _ => None,
    })
}

fn remove_in(items: &mut Vec<Item>, id: u64) -> bool {
    let before = items.len();
    items.retain(|item| match item {
        Item::Request(r) => r.id != id,
        Item::Folder(f) => f.id != id,
    });
    before != items.len()
        || items.iter_mut().any(|item| match item {
            Item::Folder(f) => remove_in(&mut f.items, id),
            _ => false,
        })
}

fn folder_in_mut(items: &mut [Item], id: u64) -> Option<&mut Folder> {
    for item in items {
        if let Item::Folder(f) = item {
            if f.id == id {
                return Some(f);
            }
            if let Some(found) = folder_in_mut(&mut f.items, id) {
                return Some(found);
            }
        }
    }
    None
}

impl Workspace {
    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn request(&self, id: u64) -> Option<(&Collection, &Request)> {
        self.collections
            .iter()
            .find_map(|c| find_in(&c.items, id).map(|r| (c, &r.request)))
    }

    pub fn request_mut(&mut self, id: u64) -> Option<&mut Request> {
        self.collections
            .iter_mut()
            .find_map(|c| find_in_mut(&mut c.items, id).map(|r| &mut r.request))
    }

    /// Collection and folder names leading to a saved request.
    pub fn breadcrumb(&self, id: u64) -> Vec<String> {
        fn walk(items: &[Item], id: u64, path: &mut Vec<String>) -> bool {
            items.iter().any(|item| match item {
                Item::Request(r) => r.id == id,
                Item::Folder(f) => {
                    path.push(f.name.clone());
                    walk(&f.items, id, path) || {
                        path.pop();
                        false
                    }
                }
            })
        }
        for c in &self.collections {
            let mut path = vec![c.name.clone()];
            if walk(&c.items, id, &mut path) {
                return path;
            }
        }
        Vec::new()
    }

    /// Removes a request, folder, or collection by id.
    pub fn remove(&mut self, id: u64) {
        self.collections.retain(|c| c.id != id);
        for c in &mut self.collections {
            if remove_in(&mut c.items, id) {
                return;
            }
        }
    }

    pub fn toggle_expanded(&mut self, id: u64) {
        if let Some(c) = self.collections.iter_mut().find(|c| c.id == id) {
            c.expanded = !c.expanded;
            return;
        }
        for c in &mut self.collections {
            if let Some(f) = folder_in_mut(&mut c.items, id) {
                f.expanded = !f.expanded;
                return;
            }
        }
    }

    pub fn add_collection(&mut self, name: impl Into<String>) -> u64 {
        let id = self.alloc_id();
        self.collections.push(Collection {
            id,
            name: name.into(),
            items: Vec::new(),
            variables: Vec::new(),
            expanded: true,
        });
        id
    }

    pub fn add_request(&mut self, collection: u64, request: Request) -> u64 {
        let id = self.alloc_id();
        if let Some(c) = self.collections.iter_mut().find(|c| c.id == collection) {
            c.expanded = true;
            c.items.push(Item::Request(SavedRequest { id, request }));
        }
        id
    }

    pub fn rename(&mut self, id: u64, name: String) {
        if let Some(c) = self.collections.iter_mut().find(|c| c.id == id) {
            c.name = name;
        } else if let Some(e) = self.environments.iter_mut().find(|e| e.id == id) {
            e.name = name;
        } else if let Some(r) = self.request_mut(id) {
            r.name = name;
        } else {
            for c in &mut self.collections {
                if let Some(f) = folder_in_mut(&mut c.items, id) {
                    f.name = name;
                    return;
                }
            }
        }
    }

    /// Variables for a send: globals, then collection variables, then the active environment; later wins.
    pub fn variables(&self, request_id: Option<u64>) -> Vec<(String, String)> {
        let enabled = |values: &[KeyValue]| {
            values
                .iter()
                .filter(|v| v.enabled && !v.key.is_empty())
                .map(|v| (v.key.clone(), v.value.clone()))
                .collect::<Vec<_>>()
        };
        let mut vars = enabled(&self.globals);
        if let Some((c, _)) = request_id.and_then(|id| self.request(id)) {
            vars.extend(enabled(&c.variables));
        }
        if let Some(env) = self
            .active_environment
            .and_then(|id| self.environments.iter().find(|e| e.id == id))
        {
            vars.extend(enabled(&env.values));
        }
        vars
    }

    /// Name and variables of an environment, a collection, or the globals (`GLOBALS_ID`).
    pub fn variable_set(&self, id: u64) -> Option<(String, &Vec<KeyValue>)> {
        if id == GLOBALS_ID {
            return Some(("Globals".into(), &self.globals));
        }
        if let Some(e) = self.environments.iter().find(|e| e.id == id) {
            return Some((e.name.clone(), &e.values));
        }
        self.collections.iter().find(|c| c.id == id).map(|c| (c.name.clone(), &c.variables))
    }

    pub fn variable_set_mut(&mut self, id: u64) -> Option<&mut Vec<KeyValue>> {
        if id == GLOBALS_ID {
            return Some(&mut self.globals);
        }
        if let Some(e) = self.environments.iter_mut().find(|e| e.id == id) {
            return Some(&mut e.values);
        }
        self.collections.iter_mut().find(|c| c.id == id).map(|c| &mut c.variables)
    }

    pub fn push_history(&mut self, entry: HistoryEntry) {
        self.history.insert(0, entry);
        self.history.truncate(HISTORY_LIMIT);
    }

    /// Imports a Postman Collection v2.0/v2.1 document. Returns the new collection id.
    pub fn import_postman(&mut self, text: &str) -> Result<u64, String> {
        let doc: Value = serde_json::from_str(text).map_err(|e| format!("Not valid JSON: {e}"))?;
        let info = doc
            .get("info")
            .ok_or("Not a Postman collection: missing \"info\"")?;
        let name = info
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Imported collection")
            .to_string();
        let collection_auth = doc.get("auth").map(auth_from_postman);
        let items = self.items_from_postman(doc.get("item"), collection_auth.as_ref());
        let id = self.alloc_id();
        self.collections.push(Collection {
            id,
            name,
            items,
            variables: kv_from_postman(doc.get("variable")),
            expanded: true,
        });
        Ok(id)
    }

    fn items_from_postman(&mut self, items: Option<&Value>, inherited: Option<&Auth>) -> Vec<Item> {
        let Some(items) = items.and_then(Value::as_array) else {
            return Vec::new();
        };
        items
            .iter()
            .map(|item| {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled")
                    .to_string();
                if item.get("item").is_some() {
                    let own = item.get("auth").map(auth_from_postman);
                    let auth = own.as_ref().or(inherited);
                    let children = self.items_from_postman(item.get("item"), auth);
                    Item::Folder(Folder {
                        id: self.alloc_id(),
                        name,
                        items: children,
                        expanded: false,
                    })
                } else {
                    let mut request =
                        request_from_postman(item.get("request").unwrap_or(&Value::Null));
                    request.name = name;
                    request.examples = examples_from_postman(item.get("response"));
                    if request.auth == Auth::None && item.pointer("/request/auth").is_none() {
                        request.auth = inherited.cloned().unwrap_or_default();
                    }
                    Item::Request(SavedRequest {
                        id: self.alloc_id(),
                        request,
                    })
                }
            })
            .collect()
    }

    pub fn export_postman(&self, collection_id: u64) -> Option<String> {
        let c = self.collections.iter().find(|c| c.id == collection_id)?;
        let doc = json!({
            "info": { "name": c.name },
            "item": items_to_postman(&c.items),
            "variable": kv_to_postman(&c.variables),
        });
        serde_json::to_string_pretty(&doc).ok()
    }
}

fn kv_from_postman(value: Option<&Value>) -> Vec<KeyValue> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let key = row.get("key").and_then(Value::as_str)?;
                    let value = match row.get("value") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(other) => other.to_string(),
                    };
                    let disabled = row.get("disabled").and_then(Value::as_bool).unwrap_or(false);
                    let file = row.get("type").and_then(Value::as_str) == Some("file");
                    let value = match row.get("src") {
                        Some(Value::String(src)) if file => src.clone(),
                        Some(Value::Array(srcs)) if file => srcs.first().and_then(Value::as_str).unwrap_or_default().into(),
                        _ => value,
                    };
                    Some(KeyValue {
                        key: key.into(),
                        value,
                        enabled: !disabled,
                        file,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn examples_from_postman(value: Option<&Value>) -> Vec<Example> {
    let Some(rows) = value.and_then(Value::as_array) else { return Vec::new() };
    rows.iter()
        .map(|r| Example {
            name: r.get("name").and_then(Value::as_str).unwrap_or("Example").into(),
            status: r.get("code").and_then(Value::as_u64).unwrap_or(200) as u16,
            headers: kv_from_postman(r.get("header")),
            body: r.get("body").and_then(Value::as_str).unwrap_or_default().into(),
        })
        .collect()
}

fn kv_to_postman(rows: &[KeyValue]) -> Value {
    rows.iter()
        .map(|r| match r.file {
            true => json!({"key": r.key, "src": r.value, "type": "file", "disabled": !r.enabled}),
            false => json!({"key": r.key, "value": r.value, "disabled": !r.enabled}),
        })
        .collect()
}

fn auth_field(auth: &Value, kind: &str, key: &str) -> String {
    let rows = auth.get(kind);
    // v2.1 stores a key/value array; v2.0 stores an object.
    rows.and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|r| r.get("key").and_then(Value::as_str) == Some(key))
        })
        .and_then(|r| r.get("value"))
        .or_else(|| rows.and_then(|o| o.get(key)))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn auth_from_postman(auth: &Value) -> Auth {
    match auth.get("type").and_then(Value::as_str) {
        Some("bearer") => Auth::Bearer {
            token: auth_field(auth, "bearer", "token"),
        },
        Some("basic") => Auth::Basic {
            username: auth_field(auth, "basic", "username"),
            password: auth_field(auth, "basic", "password"),
        },
        Some("apikey") => Auth::ApiKey {
            key: auth_field(auth, "apikey", "key"),
            value: auth_field(auth, "apikey", "value"),
            in_query: auth_field(auth, "apikey", "in") == "query",
        },
        Some("digest") => Auth::Digest {
            username: auth_field(auth, "digest", "username"),
            password: auth_field(auth, "digest", "password"),
        },
        Some("oauth2") => {
            let f = |key| auth_field(auth, "oauth2", key);
            let d = OAuth2::default();
            let or = |v: String, default: String| if v.is_empty() { default } else { v };
            Auth::OAuth2(OAuth2 {
                access_token: f("accessToken"),
                header_prefix: or(f("headerPrefix"), d.header_prefix),
                grant_type: match f("grant_type").as_str() {
                    "password_credentials" => "password".into(),
                    _ => d.grant_type,
                },
                token_url: f("accessTokenUrl"),
                client_id: f("clientId"),
                client_secret: f("clientSecret"),
                scope: f("scope"),
                username: f("username"),
                password: f("password"),
            })
        }
        Some("awsv4") => {
            let f = |key| auth_field(auth, "awsv4", key);
            Auth::AwsV4 {
                access_key: f("accessKey"),
                secret_key: f("secretKey"),
                region: f("region"),
                service: f("service"),
                session_token: f("sessionToken"),
            }
        }
        _ => Auth::None,
    }
}

fn auth_to_postman(auth: &Auth) -> Value {
    let rows = |pairs: &[(&str, &str)]| -> Value {
        pairs
            .iter()
            .map(|(k, v)| json!({"key": k, "value": v, "type": "string"}))
            .collect()
    };
    match auth {
        Auth::None => json!({"type": "noauth"}),
        Auth::Bearer { token } => json!({"type": "bearer", "bearer": rows(&[("token", token)])}),
        Auth::Basic { username, password } => json!({"type": "basic",
            "basic": rows(&[("username", username), ("password", password)])}),
        Auth::ApiKey { key, value, in_query } => json!({"type": "apikey",
            "apikey": rows(&[("key", key), ("value", value), ("in", if *in_query { "query" } else { "header" })])}),
        Auth::Digest { username, password } => json!({"type": "digest",
            "digest": rows(&[("username", username), ("password", password)])}),
        Auth::OAuth2(o) => json!({"type": "oauth2", "oauth2": rows(&[
            ("accessToken", &o.access_token),
            ("headerPrefix", &o.header_prefix),
            ("grant_type", if o.grant_type == "password" { "password_credentials" } else { "client_credentials" }),
            ("accessTokenUrl", &o.token_url),
            ("clientId", &o.client_id),
            ("clientSecret", &o.client_secret),
            ("scope", &o.scope),
            ("username", &o.username),
            ("password", &o.password),
        ])}),
        Auth::AwsV4 { access_key, secret_key, region, service, session_token } => json!({"type": "awsv4", "awsv4": rows(&[
            ("accessKey", access_key),
            ("secretKey", secret_key),
            ("region", region),
            ("service", service),
            ("sessionToken", session_token),
        ])}),
    }
}

fn request_from_postman(value: &Value) -> Request {
    let mut request = Request::default();
    if let Some(url) = value.as_str() {
        // v2.1 also allows a request that is just a URL string.
        request.set_url(url.into());
        return request;
    }
    request.method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_uppercase();
    let url = match value.get("url") {
        Some(Value::String(s)) => s.clone(),
        Some(obj) => obj
            .get("raw")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        None => String::new(),
    };
    request.set_url(url);
    if let Some(query) = value.pointer("/url/query") {
        let disabled = kv_from_postman(Some(query))
            .into_iter()
            .filter(|kv| !kv.enabled);
        request.params.extend(disabled);
    }
    request.headers = kv_from_postman(value.get("header"));
    request.description = match value.get("description") {
        Some(Value::String(s)) => s.clone(),
        Some(d) => d.get("content").and_then(Value::as_str).unwrap_or_default().into(),
        None => String::new(),
    };
    if let Some(auth) = value.get("auth") {
        request.auth = auth_from_postman(auth);
    }
    if let Some(body) = value.get("body") {
        match body.get("mode").and_then(Value::as_str) {
            Some("raw") => {
                request.body_mode = BodyMode::Raw;
                request.body_raw = body
                    .get("raw")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into();
                request.raw_language = body
                    .pointer("/options/raw/language")
                    .and_then(Value::as_str)
                    .map(RawLanguage::from_postman)
                    .unwrap_or(RawLanguage::Text);
            }
            Some("urlencoded") => {
                request.body_mode = BodyMode::UrlEncoded;
                request.body_form = kv_from_postman(body.get("urlencoded"));
            }
            Some("formdata") => {
                request.body_mode = BodyMode::FormData;
                request.body_multipart = kv_from_postman(body.get("formdata"));
            }
            Some("graphql") => {
                request.body_mode = BodyMode::GraphQl;
                let field = |name: &str| body.pointer(&format!("/graphql/{name}")).and_then(Value::as_str).unwrap_or_default().to_string();
                request.graphql_query = field("query");
                request.graphql_variables = field("variables");
            }
            Some("file") => {
                request.body_mode = BodyMode::Binary;
                request.body_file = body.pointer("/file/src").and_then(Value::as_str).unwrap_or_default().into();
            }
            _ => {}
        }
    }
    request
}

fn request_to_postman(r: &Request) -> Value {
    let mut out = json!({
        "method": r.method,
        "header": kv_to_postman(&r.headers),
        "url": {"raw": r.url, "query": kv_to_postman(&r.params)},
        "auth": auth_to_postman(&r.auth),
        "description": r.description,
    });
    match r.body_mode {
        BodyMode::None => {}
        BodyMode::Raw => {
            out["body"] = json!({"mode": "raw", "raw": r.body_raw,
                "options": {"raw": {"language": r.raw_language.postman_name()}}})
        }
        BodyMode::UrlEncoded => {
            out["body"] = json!({"mode": "urlencoded", "urlencoded": kv_to_postman(&r.body_form)})
        }
        BodyMode::FormData => {
            out["body"] = json!({"mode": "formdata", "formdata": kv_to_postman(&r.body_multipart)})
        }
        BodyMode::GraphQl => {
            out["body"] = json!({"mode": "graphql",
                "graphql": {"query": r.graphql_query, "variables": r.graphql_variables}})
        }
        BodyMode::Binary => out["body"] = json!({"mode": "file", "file": {"src": r.body_file}}),
    }
    out
}

fn items_to_postman(items: &[Item]) -> Value {
    items
        .iter()
        .map(|item| match item {
            Item::Folder(f) => json!({"name": f.name, "item": items_to_postman(&f.items)}),
            Item::Request(r) => json!({
                "name": r.request.name,
                "request": request_to_postman(&r.request),
                "response": r.request.examples.iter().map(|e| json!({
                    "name": e.name,
                    "code": e.status,
                    "header": kv_to_postman(&e.headers),
                    "body": e.body,
                })).collect::<Vec<_>>(),
            }),
        })
        .collect()
}

/// Parses JSON, also accepting JavaScript object literals: unquoted keys,
/// single-quoted strings, trailing commas and comments. Used by Beautify.
pub fn parse_relaxed_json(source: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str(source) {
        return Ok(value);
    }
    let mut out = String::with_capacity(source.len());
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    let skip_blank = |mut j: usize| {
        loop {
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'/') {
                while j < chars.len() && chars[j] != '\n' {
                    j += 1;
                }
            } else if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'*') {
                j += 2;
                while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                    j += 1;
                }
                j += 2;
            } else {
                return j;
            }
        }
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                out.push('"');
                i += 1;
                while i < chars.len() && chars[i] != c {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            if chars[i + 1] == '\'' {
                                out.push('\'');
                            } else {
                                out.push('\\');
                                out.push(chars[i + 1]);
                            }
                            i += 2;
                            continue;
                        }
                        '"' => out.push_str("\\\""),
                        ch => out.push(ch),
                    }
                    i += 1;
                }
                out.push('"');
                i += 1;
            }
            '/' if matches!(chars.get(i + 1), Some('/') | Some('*')) => i = skip_blank(i),
            ',' => {
                let next = skip_blank(i + 1);
                if !matches!(chars.get(next), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if chars.get(skip_blank(i)) == Some(&':') {
                    out.push('"');
                    out.push_str(&word);
                    out.push('"');
                } else {
                    out.push_str(&word);
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    serde_json::from_str(&out).map_err(|e| e.to_string())
}

/// Splits a shell command line into words, honouring quotes and backslash-newline continuations.
fn shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // Bash ANSI-C quoting, used by browsers' "Copy as cURL" for bodies with special characters.
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => word.push('\n'),
                            Some('t') => word.push('\t'),
                            Some('r') => word.push('\r'),
                            Some(c) => word.push(c),
                            None => return Err("Unclosed $' quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("Unclosed $' quote".into()),
                    }
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("Unclosed single quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => word.push(c),
                            Some('\n') => {}
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("Unclosed double quote".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("Unclosed double quote".into()),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | Some('\r') => {}
                Some(c) => {
                    in_word = true;
                    word.push(c);
                }
                None => {}
            },
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Parses a `curl` command into a request when one is pasted into the URL bar.
pub fn parse_curl(command: &str) -> Result<Request, String> {
    let words = shell_words(command.trim())?;
    let mut args = words.into_iter();
    match args.next() {
        Some(first) if first == "curl" => {}
        _ => return Err("Not a curl command".into()),
    }
    let mut request = Request::default();
    let mut method: Option<String> = None;
    let mut data: Vec<String> = Vec::new();
    let mut parts: Vec<KeyValue> = Vec::new();
    let mut url = String::new();
    let mut get_query = false;
    let mut digest = false;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-X" | "--request" => method = Some(value(&arg)?.to_uppercase()),
            "-H" | "--header" => {
                let header = value(&arg)?;
                if let Some((k, v)) = header.split_once(':') {
                    request.headers.push(KeyValue::new(k.trim(), v.trim()));
                }
            }
            "--data-binary" => {
                let v = value(&arg)?;
                match v.strip_prefix('@') {
                    Some(path) => request.body_file = path.into(),
                    None => data.push(v),
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-ascii" | "--data-urlencode" => data.push(value(&arg)?),
            "--digest" => digest = true,
            "-F" | "--form" | "--form-string" => {
                let field = value(&arg)?;
                let (k, v) = field.split_once('=').unwrap_or((&field, ""));
                let file = arg != "--form-string" && v.starts_with('@');
                // curl's ";type=..." part options are not modelled.
                let v = match v.strip_prefix("@\"") {
                    // curl's quoted form allows `;` and spaces in the file name.
                    Some(quoted) if file => quoted.split('"').next().unwrap_or_default(),
                    _ if file => v[1..].split(';').next().unwrap_or_default(),
                    _ => v,
                };
                parts.push(KeyValue { file, ..KeyValue::new(k, v) });
            }
            "-u" | "--user" => {
                let user = value(&arg)?;
                let (username, password) = user.split_once(':').unwrap_or((&user, ""));
                request.auth = Auth::Basic {
                    username: username.into(),
                    password: password.into(),
                };
            }
            "-A" | "--user-agent" => request.headers.push(KeyValue::new("User-Agent", value(&arg)?)),
            "-b" | "--cookie" => request.headers.push(KeyValue::new("Cookie", value(&arg)?)),
            "-e" | "--referer" => request.headers.push(KeyValue::new("Referer", value(&arg)?)),
            "--url" => url = value(&arg)?,
            "-I" | "--head" => method = Some("HEAD".into()),
            "-G" | "--get" => get_query = true,
            "-k" | "--insecure" => request.verify_ssl = false,
            // Flags with an argument that do not affect the request model.
            "-o" | "--output" | "-m" | "--max-time" | "--connect-timeout" | "-w" | "--write-out"
            | "-x" | "--proxy" => {
                value(&arg)?;
            }
            a if a.starts_with('-') => {}
            _ => url = arg,
        }
    }
    if url.is_empty() {
        return Err("curl command has no URL".into());
    }
    if get_query && !data.is_empty() {
        let sep = if url.contains('?') { '&' } else { '?' };
        url = format!("{url}{sep}{}", data.join("&"));
        data.clear();
    }
    request.set_url(url);
    if !data.is_empty() {
        let body = data.join("&");
        let content_type = request
            .headers
            .iter()
            .find(|h| h.key.eq_ignore_ascii_case("content-type"))
            .map(|h| h.value.to_ascii_lowercase());
        if content_type.as_deref() == Some("application/x-www-form-urlencoded")
            || (content_type.is_none() && !body.trim_start().starts_with(['{', '[']))
        {
            request.body_mode = BodyMode::UrlEncoded;
            request.body_form = params_from_url(&format!("?{body}"), &[]);
            request
                .headers
                .retain(|h| !h.key.eq_ignore_ascii_case("content-type"));
        } else {
            request.body_mode = BodyMode::Raw;
            request.raw_language = if content_type.as_deref().is_some_and(|c| !c.contains("json"))
            {
                RawLanguage::Text
            } else {
                RawLanguage::Json
            };
            request.body_raw = body;
        }
    }
    if !request.body_file.is_empty() && request.body_mode == BodyMode::None {
        request.body_mode = BodyMode::Binary;
    }
    if let (true, Auth::Basic { username, password }) = (digest, &request.auth) {
        request.auth = Auth::Digest { username: username.clone(), password: password.clone() };
    }
    if !parts.is_empty() {
        request.body_mode = BodyMode::FormData;
        request.body_multipart = parts;
        // curl generates the multipart boundary; a pasted fixed one would be wrong.
        request.headers.retain(|h| !h.key.eq_ignore_ascii_case("content-type"));
    }
    request.method = method.unwrap_or_else(|| {
        if request.body_mode == BodyMode::None {
            "GET".into()
        } else {
            "POST".into()
        }
    });
    Ok(request)
}

/// The JSON payload of a GraphQL body. Invalid variables JSON is sent as a string so the server reports it.
pub fn graphql_body(r: &Request, vars: &[(String, String)]) -> String {
    let variables = resolve(&r.graphql_variables, vars);
    let variables = match variables.trim() {
        "" => json!({}),
        text => serde_json::from_str(text).unwrap_or(Value::String(variables)),
    };
    json!({"query": resolve(&r.graphql_query, vars), "variables": variables}).to_string()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Generates a cURL snippet with variables resolved.
pub fn to_curl(r: &Request, vars: &[(String, String)]) -> String {
    let mut parts = vec![format!(
        "curl --location --request {} {}",
        r.method,
        shell_quote(&resolve(&r.url, vars))
    )];
    let header = |k: &str, v: &str| format!("--header {}", shell_quote(&format!("{k}: {v}")));
    for h in r.headers.iter().filter(|h| h.enabled && !h.key.is_empty()) {
        parts.push(header(&resolve(&h.key, vars), &resolve(&h.value, vars)));
    }
    match &r.auth {
        Auth::Bearer { token } => {
            parts.push(header("Authorization", &format!("Bearer {}", resolve(token, vars))))
        }
        Auth::Basic { username, password } => parts.push(format!(
            "--user {}",
            shell_quote(&format!("{}:{}", resolve(username, vars), resolve(password, vars)))
        )),
        Auth::ApiKey {
            key,
            value,
            in_query: false,
        } if !key.is_empty() => parts.push(header(&resolve(key, vars), &resolve(value, vars))),
        Auth::Digest { username, password } => parts.push(format!(
            "--digest --user {}",
            shell_quote(&format!("{}:{}", resolve(username, vars), resolve(password, vars)))
        )),
        Auth::OAuth2(o) if !o.access_token.is_empty() => parts.push(header(
            "Authorization",
            format!("{} {}", resolve(&o.header_prefix, vars), resolve(&o.access_token, vars)).trim(),
        )),
        Auth::AwsV4 { access_key, secret_key, region, service, .. } => {
            let scope = format!("aws:amz:{}:{}", resolve(region, vars), resolve(service, vars));
            parts.push(format!("--aws-sigv4 {}", shell_quote(&scope)));
            parts.push(format!(
                "--user {}",
                shell_quote(&format!("{}:{}", resolve(access_key, vars), resolve(secret_key, vars)))
            ));
        }
        _ => {}
    }
    match r.body_mode {
        BodyMode::None => {}
        BodyMode::Raw => parts.push(format!("--data {}", shell_quote(&resolve(&r.body_raw, vars)))),
        BodyMode::UrlEncoded => {
            for f in r.body_form.iter().filter(|f| f.enabled && !f.key.is_empty()) {
                parts.push(format!(
                    "--data-urlencode {}",
                    shell_quote(&format!("{}={}", resolve(&f.key, vars), resolve(&f.value, vars)))
                ));
            }
        }
        BodyMode::FormData => {
            for f in r.body_multipart.iter().filter(|f| f.enabled && !f.key.is_empty()) {
                let value = resolve(&f.value, vars);
                let value = if f.file { format!("@\"{value}\"") } else { value };
                parts.push(format!("--form {}", shell_quote(&format!("{}={value}", resolve(&f.key, vars)))));
            }
        }
        BodyMode::GraphQl => {
            parts.push(header("Content-Type", "application/json"));
            parts.push(format!("--data {}", shell_quote(&graphql_body(r, vars))));
        }
        BodyMode::Binary => {
            parts.push(format!("--data-binary {}", shell_quote(&format!("@{}", resolve(&r.body_file, vars)))))
        }
    }
    parts.join(" \\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolve_replaces_known_and_keeps_unknown() {
        let v = vars(&[("base", "https://x.test"), ("id", "7")]);
        assert_eq!(resolve("{{base}}/u/{{ id }}", &v), "https://x.test/u/7");
        assert_eq!(resolve("{{missing}}/{{id}}", &v), "{{missing}}/7");
        assert_eq!(resolve("open {{brace", &v), "open {{brace");
    }

    #[test]
    fn form_data_and_graphql_round_trip() {
        let r = parse_curl("curl https://x.test/up -H 'Content-Type: multipart/form-data; boundary=x' -F name=Ada -F 'doc=@/tmp/a b.txt;type=text/plain'").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body_mode, BodyMode::FormData);
        assert_eq!(r.body_multipart, vec![KeyValue::new("name", "Ada"), KeyValue { file: true, ..KeyValue::new("doc", "/tmp/a b.txt") }]);
        assert!(r.headers.is_empty(), "the pasted boundary is dropped");
        let curl = to_curl(&r, &[]);
        assert!(curl.contains("--form 'name=Ada'") && curl.contains(r#"--form 'doc=@"/tmp/a b.txt"'"#), "{curl}");
        assert_eq!(parse_curl(&curl).unwrap().body_multipart, r.body_multipart);

        let mut g = Request::default();
        g.method = "POST".into();
        g.body_mode = BodyMode::GraphQl;
        g.graphql_query = "{ me { id } }".into();
        g.graphql_variables = "{\"a\": {{n}}}".into();
        assert_eq!(graphql_body(&g, &vars(&[("n", "1")])), r#"{"query":"{ me { id } }","variables":{"a":1}}"#);
        g.graphql_variables = "nope".into();
        assert!(graphql_body(&g, &[]).contains(r#""variables":"nope""#));

        for request in [r, g] {
            let back = request_from_postman(&request_to_postman(&request));
            assert_eq!((back.body_mode, &back.body_multipart, &back.graphql_query, &back.graphql_variables),
                (request.body_mode, &request.body_multipart, &request.graphql_query, &request.graphql_variables));
        }
    }

    #[test]
    fn new_auth_types_binary_body_docs_and_examples_round_trip() {
        let mut oauth = OAuth2 { access_token: "tok".into(), token_url: "https://id.test/token".into(), ..OAuth2::default() };
        oauth.grant_type = "password".into();
        let aws = Auth::AwsV4 {
            access_key: "AK".into(),
            secret_key: "SK".into(),
            region: "eu-west-1".into(),
            service: "s3".into(),
            session_token: String::new(),
        };
        for auth in [Auth::Digest { username: "u".into(), password: "p".into() }, Auth::OAuth2(oauth), aws] {
            let mut r = Request { auth, ..Request::default() };
            r.body_mode = BodyMode::Binary;
            r.body_file = "/tmp/blob.bin".into();
            r.description = "Uploads a blob.".into();
            r.examples = vec![Example { name: "ok".into(), status: 201, headers: vec![KeyValue::new("X", "1")], body: "{}".into() }];
            let item = json!([{"name": "r", "request": request_to_postman(&r), "response": items_to_postman(&[Item::Request(SavedRequest { id: 1, request: r.clone() })])[0]["response"]}]);
            let mut ws = Workspace::default();
            let id = ws.import_postman(&json!({"info": {"name": "c"}, "item": item}).to_string()).unwrap();
            let back = match &ws.collections.iter().find(|c| c.id == id).unwrap().items[0] {
                Item::Request(saved) => saved.request.clone(),
                _ => unreachable!(),
            };
            assert_eq!((&back.auth, back.body_mode, &back.body_file, &back.description, &back.examples),
                (&r.auth, r.body_mode, &r.body_file, &r.description, &r.examples));
            assert_eq!(Auth::from_index(r.auth.index()).label(), r.auth.label());
        }
        let r = parse_curl("curl --digest -u ada:pw --data-binary @/tmp/x.bin https://x.test").unwrap();
        assert_eq!(r.auth, Auth::Digest { username: "ada".into(), password: "pw".into() });
        assert_eq!((r.body_mode, r.body_file.as_str(), r.method.as_str()), (BodyMode::Binary, "/tmp/x.bin", "POST"));
        let curl = to_curl(&r, &[]);
        assert!(curl.contains("--digest --user 'ada:pw'") && curl.contains("--data-binary '@/tmp/x.bin'"), "{curl}");
    }

    #[test]
    fn shared_data_round_trips_and_keeps_local_parts() {
        let mut ws = Workspace::default();
        let c = ws.add_collection("API");
        ws.add_request(c, Request::default());
        ws.globals.push(KeyValue::new("host", "x"));
        ws.environments.push(Environment { id: 99, name: "Dev".into(), values: vec![] });
        ws.active_environment = Some(99);
        ws.push_history(HistoryEntry { request: Request::default(), status: Some(200), at_unix_ms: 1 });
        let json = serde_json::to_value(ws.shared()).unwrap();

        let mut other = Workspace { next_id: 50, ..Workspace::default() };
        other.active_environment = Some(7);
        other.apply_shared(serde_json::from_value(json).unwrap());
        assert_eq!(other.shared().collections, ws.collections);
        assert_eq!(other.next_id, 50, "never moves ids backwards");
        assert_eq!(other.active_environment, None, "an environment that no longer exists is deselected");
        assert!(other.history.is_empty(), "history stays on each machine");

        let app = AppData {
            workspaces: vec![
                WorkspaceMeta { id: "1".into(), name: "a".into(), remote: None },
                WorkspaceMeta { id: "7".into(), name: "b".into(), remote: None },
            ],
            ..AppData::default()
        };
        assert_eq!(app.new_local_id(), "8");
    }

    #[test]
    fn environment_overrides_collection_overrides_globals() {
        let mut ws = Workspace::default();
        ws.globals = vec![KeyValue::new("a", "global"), KeyValue::new("b", "global")];
        let c = ws.add_collection("c");
        ws.variable_set_mut(c).unwrap().push(KeyValue::new("a", "collection"));
        let r = ws.add_request(c, Request::default());
        let resolved = resolve("{{a}} {{b}}", &ws.variables(Some(r)));
        assert_eq!(resolved, "collection global");
        assert_eq!(ws.variable_set(GLOBALS_ID).unwrap().0, "Globals");
    }

    #[test]
    fn decodes_binary_messages() {
        assert_eq!(BinaryEncoding::Hex.decode("0x00 ff 10").unwrap(), vec![0, 255, 16]);
        assert!(BinaryEncoding::Hex.decode("abc").is_err());
        assert_eq!(BinaryEncoding::Base64.decode("aGk=\n").unwrap(), b"hi");
        assert!(BinaryEncoding::Base64.decode("*").is_err());
    }

    #[test]
    fn url_scheme_switches_between_http_and_websocket() {
        let mut r = Request::default();
        r.set_url("WSS://example.com/live".into());
        assert!(r.is_websocket());
        r.set_url("example.com".into());
        assert!(r.is_websocket(), "no scheme keeps the current kind");
        r.set_url("https://example.com".into());
        assert_eq!(r.method, "GET");
        r.method = "POST".into();
        r.set_url("http://example.com".into());
        assert_eq!(r.method, "POST", "plain HTTP URLs leave the method alone");
    }

    #[test]
    fn params_and_url_stay_in_sync() {
        let mut r = Request::default();
        r.set_url("https://a.test/p?x=1&y#frag".into());
        assert_eq!(r.params, vec![KeyValue::new("x", "1"), KeyValue::new("y", "")]);
        let mut params = r.params.clone();
        params[0].enabled = false;
        params.push(KeyValue::new("z", "{{v}}"));
        r.set_params(params);
        assert_eq!(r.url, "https://a.test/p?y&z={{v}}#frag");
        // Editing the URL keeps the disabled row around.
        r.set_url("https://a.test/p?q=2".into());
        assert_eq!(r.params.len(), 2);
        assert!(!r.params[1].enabled && r.params[1].key == "x");
    }

    #[test]
    fn relaxed_json_accepts_javascript_objects() {
        let v = parse_relaxed_json(
            r#"{
  title: 'foo', // note
  body: 'it\'s "bar"',
  userId: 1,
  tags: ['a', 'b',],
}"#,
        )
        .unwrap();
        assert_eq!(v["title"], "foo");
        assert_eq!(v["body"], "it's \"bar\"");
        assert_eq!(v["userId"], 1);
        assert_eq!(v["tags"][1], "b");
        assert!(parse_relaxed_json("{ title: }").is_err());
        assert_eq!(parse_relaxed_json("[1, true, null]").unwrap()[1], true);
    }

    #[test]
    fn parses_browser_copy_as_curl() {
        let r = parse_curl(
            "curl 'https://api.test/posts' \\\n  -H 'accept: */*' \\\n  -H 'content-type: application/json' \\\n  --data-raw $'{\"title\":\"it\\'s\"}' \\\n  --compressed",
        )
        .unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://api.test/posts");
        assert_eq!(r.headers.len(), 2);
        assert_eq!(r.body_raw, "{\"title\":\"it's\"}");
    }

    #[test]
    fn parses_pasted_curl() {
        let r = parse_curl(
            "curl -X PUT 'https://api.test/items/1?draft=true' \\\n  -H 'Content-Type: application/json' \\\n  -H \"Authorization: Bearer abc\" \\\n  --data-raw '{\"name\":\"x\"}'",
        )
        .unwrap();
        assert_eq!(r.method, "PUT");
        assert_eq!(r.params, vec![KeyValue::new("draft", "true")]);
        assert_eq!(r.headers.len(), 2);
        assert_eq!(r.body_mode, BodyMode::Raw);
        assert_eq!(r.body_raw, "{\"name\":\"x\"}");

        let r = parse_curl("curl https://a.test -d a=1 -d b=2 -u me:pw").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body_mode, BodyMode::UrlEncoded);
        assert_eq!(r.body_form, vec![KeyValue::new("a", "1"), KeyValue::new("b", "2")]);
        assert_eq!(
            r.auth,
            Auth::Basic {
                username: "me".into(),
                password: "pw".into()
            }
        );
        assert!(parse_curl("wget x").is_err());
    }

    #[test]
    fn curl_round_trip() {
        let mut r = Request::default();
        r.method = "POST".into();
        r.set_url("{{base}}/it's".into());
        r.headers.push(KeyValue::new("X-A", "1"));
        r.body_mode = BodyMode::Raw;
        r.body_raw = "{\"a\":1}".into();
        let snippet = to_curl(&r, &vars(&[("base", "http://h")]));
        let back = parse_curl(&snippet).unwrap();
        assert_eq!(back.url, "http://h/it's");
        assert_eq!(back.method, "POST");
        assert_eq!(back.body_raw, r.body_raw);
    }

    #[test]
    fn postman_import_export_round_trip() {
        let doc = r#"{
          "info": {"name": "Demo", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"},
          "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "{{tok}}"}]},
          "variable": [{"key": "base", "value": "http://localhost:5000"}],
          "item": [
            {"name": "List", "request": {"method": "GET", "url": {"raw": "{{base}}/a?x=1", "query": [{"key": "x", "value": "1"}, {"key": "off", "value": "2", "disabled": true}]}}},
            {"name": "Folder", "item": [
              {"name": "Create", "request": {"method": "POST", "header": [{"key": "Content-Type", "value": "application/json"}],
                "auth": {"type": "noauth"},
                "body": {"mode": "raw", "raw": "{}", "options": {"raw": {"language": "json"}}}, "url": "{{base}}/a"}}
            ]}
          ]
        }"#;
        let mut ws = Workspace::default();
        let id = ws.import_postman(doc).unwrap();
        let c = &ws.collections[0];
        assert_eq!(c.name, "Demo");
        assert_eq!(c.variables, vec![KeyValue::new("base", "http://localhost:5000")]);
        let Item::Request(list) = &c.items[0] else { panic!() };
        assert_eq!(list.request.params.len(), 2);
        assert_eq!(list.request.auth, Auth::Bearer { token: "{{tok}}".into() });
        let Item::Folder(folder) = &c.items[1] else { panic!() };
        let Item::Request(create) = &folder.items[0] else { panic!() };
        assert_eq!(create.request.method, "POST");
        assert_eq!(create.request.auth, Auth::None);
        assert_eq!(create.request.raw_language, RawLanguage::Json);

        assert_eq!(
            ws.variables(Some(list.id)),
            vec![("base".to_string(), "http://localhost:5000".to_string())]
        );

        let exported = ws.export_postman(id).unwrap();
        assert!(!exported.contains("postman.com"));
        let mut again = Workspace::default();
        again.import_postman(&exported).unwrap();
        assert_eq!(again.collections[0].items.len(), 2);
        let Item::Folder(f2) = &again.collections[0].items[1] else { panic!() };
        let Item::Request(c2) = &f2.items[0] else { panic!() };
        assert_eq!(c2.request.body_raw, "{}");
    }

    #[test]
    fn workspace_tree_operations() {
        let mut ws = Workspace::default();
        let c = ws.add_collection("C");
        let r = ws.add_request(c, Request::default());
        ws.rename(r, "Renamed".into());
        assert_eq!(ws.request(r).unwrap().1.name, "Renamed");
        ws.remove(r);
        assert!(ws.request(r).is_none());
        ws.remove(c);
        assert!(ws.collections.is_empty());
    }
}
