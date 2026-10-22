use crate::model::{graphql_body, resolve, Auth, BodyMode, Example, OAuth2, Request, Settings};
use std::io::Read;
use std::time::{Duration, Instant};

pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires: String,
    pub http_only: bool,
    pub secure: bool,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub elapsed_ms: u128,
    /// Header and body bytes as received.
    pub size: usize,
    pub headers: Vec<(String, String)>,
    pub cookies: Vec<Cookie>,
    pub content_type: String,
    pub body: Vec<u8>,
    pub truncated: bool,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn is_json(&self) -> bool {
        self.content_type.contains("json")
            || matches!(self.body.iter().find(|b| !b.is_ascii_whitespace()), Some(b'{' | b'['))
    }

    /// Pretty-printed body when it is JSON, otherwise the raw text.
    pub fn pretty(&self) -> String {
        if self.is_json() {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&self.body) {
                if let Ok(s) = serde_json::to_string_pretty(&value) {
                    return s;
                }
            }
        }
        self.text()
    }

    pub fn language(&self) -> &'static str {
        if self.is_json() {
            "json"
        } else if self.content_type.contains("html") || self.content_type.contains("xml") {
            "html"
        } else if self.content_type.contains("javascript") {
            "javascript"
        } else {
            "text"
        }
    }
}

impl From<&Example> for Response {
    fn from(e: &Example) -> Self {
        let headers: Vec<(String, String)> = e.headers.iter().map(|h| (h.key.clone(), h.value.clone())).collect();
        let content_type = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.to_ascii_lowercase())
            .unwrap_or_default();
        Response {
            status: e.status,
            reason: reqwest::StatusCode::from_u16(e.status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or_default()
                .into(),
            elapsed_ms: 0,
            size: e.body.len(),
            headers,
            cookies: Vec::new(),
            content_type,
            body: e.body.clone().into_bytes(),
            truncated: false,
        }
    }
}

pub fn format_size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.2} KB", b as f64 / 1024.0),
        b => format!("{:.2} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

fn parse_cookie(raw: &str) -> Option<Cookie> {
    let mut parts = raw.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let mut cookie = Cookie {
        name: name.trim().into(),
        value: value.trim().into(),
        domain: String::new(),
        path: String::new(),
        expires: "Session".into(),
        http_only: false,
        secure: false,
    };
    for attr in parts {
        let (k, v) = attr.split_once('=').unwrap_or((attr, ""));
        match k.trim().to_ascii_lowercase().as_str() {
            "domain" => cookie.domain = v.trim().into(),
            "path" => cookie.path = v.trim().into(),
            "expires" => cookie.expires = v.trim().into(),
            "max-age" if cookie.expires == "Session" => {
                cookie.expires = format!("in {}s", v.trim())
            }
            "httponly" => cookie.http_only = true,
            "secure" => cookie.secure = true,
            _ => {}
        }
    }
    Some(cookie)
}

fn has_header(request: &Request, name: &str) -> bool {
    request
        .headers
        .iter()
        .any(|h| h.enabled && h.key.trim().eq_ignore_ascii_case(name))
}

fn client(request: &Request, settings: &Settings) -> Result<reqwest::blocking::Client, String> {
    let read = |path: &str| std::fs::read(path.trim()).map_err(|e| format!("Could not read \"{}\": {e}", path.trim()));
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(None)
        .redirect(if request.follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .danger_accept_invalid_certs(!request.verify_ssl);
    if !settings.proxy.trim().is_empty() {
        let proxy = reqwest::Proxy::all(settings.proxy.trim()).map_err(|e| format!("Invalid proxy: {e}"))?;
        builder = builder.proxy(proxy);
    }
    if !settings.ca_cert.trim().is_empty() {
        let certs = reqwest::Certificate::from_pem_bundle(&read(&settings.ca_cert)?)
            .map_err(|e| format!("Invalid CA certificate: {e}"))?;
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    if !settings.client_cert.trim().is_empty() {
        let mut pem = read(&settings.client_cert)?;
        if !settings.client_key.trim().is_empty() {
            pem.push(b'\n');
            pem.extend(read(&settings.client_key)?);
        }
        let identity = reqwest::Identity::from_pem(&pem).map_err(|e| format!("Invalid client certificate: {e}"))?;
        builder = builder.identity(identity);
    }
    builder.build().map_err(|e| e.to_string())
}

/// Requests a token with the client-credentials or password grant ("Get New Access Token").
pub fn fetch_oauth2_token(o: &OAuth2, vars: &[(String, String)], settings: &Settings) -> Result<String, String> {
    let r = |s: &str| resolve(s.trim(), vars);
    if o.token_url.trim().is_empty() {
        return Err("Enter an Access Token URL".into());
    }
    let mut form = vec![("grant_type", o.grant_type.clone())];
    if !o.scope.trim().is_empty() {
        form.push(("scope", r(&o.scope)));
    }
    if o.grant_type == "password" {
        form.push(("username", r(&o.username)));
        form.push(("password", r(&o.password)));
    }
    let response = client(&Request::default(), settings)?
        .post(r(&o.token_url))
        .basic_auth(r(&o.client_id), Some(r(&o.client_secret)))
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .map_err(describe_error)?;
    let status = response.status();
    let text = response.text().map_err(describe_error)?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("access_token")?.as_str().map(String::from))
        .ok_or_else(|| format!("Token request failed ({status}): {text}"))
}

/// Sends `request` with `{{variables}}` resolved. Blocking; call it off the UI thread.
pub fn execute(request: &Request, vars: &[(String, String)], settings: &Settings) -> Result<Response, String> {
    let mut url = resolve(request.url.trim(), vars);
    if url.is_empty() {
        return Err("Enter a URL to send a request".into());
    }
    if !url.contains("://") {
        // People often type a bare host; assume http:// rather than failing.
        url = format!("http://{url}");
    }
    let mut parsed = url::Url::parse(&url).map_err(|e| format!("Invalid URL \"{url}\": {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("Unsupported protocol \"{}\"", parsed.scheme()));
    }
    if let Auth::ApiKey {
        key,
        value,
        in_query: true,
    } = &request.auth
    {
        if !key.is_empty() {
            parsed
                .query_pairs_mut()
                .append_pair(&resolve(key, vars), &resolve(value, vars));
        }
    }
    let method = reqwest::Method::from_bytes(request.method.trim().to_uppercase().as_bytes())
        .map_err(|_| format!("Invalid method \"{}\"", request.method))?;
    let client = client(request, settings)?;
    let mut builder = client.request(method, parsed);
    if !has_header(request, "user-agent") {
        builder = builder.header("User-Agent", concat!("FlokiRuntime/", env!("CARGO_PKG_VERSION")));
    }
    if !has_header(request, "accept") {
        builder = builder.header("Accept", "*/*");
    }
    for h in request.headers.iter().filter(|h| h.enabled && !h.key.trim().is_empty()) {
        builder = builder.header(resolve(h.key.trim(), vars), resolve(&h.value, vars));
    }
    match &request.auth {
        Auth::None => {}
        Auth::Bearer { token } => {
            builder = builder.bearer_auth(resolve(token, vars));
        }
        Auth::Basic { username, password } => {
            builder = builder.basic_auth(resolve(username, vars), Some(resolve(password, vars)));
        }
        Auth::ApiKey {
            key,
            value,
            in_query: false,
        } if !key.is_empty() => {
            builder = builder.header(resolve(key, vars), resolve(value, vars));
        }
        Auth::OAuth2(o) if !o.access_token.trim().is_empty() => {
            let value = format!("{} {}", resolve(o.header_prefix.trim(), vars), resolve(o.access_token.trim(), vars));
            builder = builder.header("Authorization", value.trim());
        }
        Auth::ApiKey { .. } | Auth::OAuth2(_) | Auth::Digest { .. } | Auth::AwsV4 { .. } => {}
    }
    match request.body_mode {
        BodyMode::None => {}
        BodyMode::Raw => {
            if !has_header(request, "content-type") {
                builder = builder.header("Content-Type", request.raw_language.content_type());
            }
            builder = builder.body(resolve(&request.body_raw, vars));
        }
        BodyMode::UrlEncoded => {
            let pairs: Vec<(String, String)> = request
                .body_form
                .iter()
                .filter(|f| f.enabled && !f.key.is_empty())
                .map(|f| (resolve(&f.key, vars), resolve(&f.value, vars)))
                .collect();
            builder = builder.form(&pairs);
        }
        BodyMode::FormData => {
            let mut form = reqwest::blocking::multipart::Form::new();
            for f in request.body_multipart.iter().filter(|f| f.enabled && !f.key.is_empty()) {
                let (key, value) = (resolve(&f.key, vars), resolve(&f.value, vars));
                form = if f.file {
                    form.file(key, &value).map_err(|e| format!("Could not read file \"{value}\": {e}"))?
                } else {
                    form.text(key, value)
                };
            }
            builder = builder.multipart(form);
        }
        BodyMode::GraphQl => {
            if !has_header(request, "content-type") {
                builder = builder.header("Content-Type", "application/json");
            }
            builder = builder.body(graphql_body(request, vars));
        }
        BodyMode::Binary => {
            let path = resolve(request.body_file.trim(), vars);
            if path.is_empty() {
                return Err("Select a file to send as the body".into());
            }
            builder = builder.body(std::fs::read(&path).map_err(|e| format!("Could not read file \"{path}\": {e}"))?);
        }
    }

    let mut outgoing = builder.build().map_err(describe_error)?;
    if let Auth::AwsV4 { access_key, secret_key, region, service, session_token } = &request.auth {
        let r = |s: &String| resolve(s.trim(), vars);
        sign_aws(&mut outgoing, &r(access_key), &r(secret_key), &r(region), &r(service), &r(session_token))?;
    }
    // Digest needs the server's nonce, so the first attempt is expected to get a 401.
    let retry = matches!(request.auth, Auth::Digest { .. }).then(|| outgoing.try_clone()).flatten();
    let start = Instant::now();
    let mut response = client.execute(outgoing).map_err(describe_error)?;
    if let (Some(mut retry), Auth::Digest { username, password }) = (retry, &request.auth) {
        let challenge = response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.trim_start().to_ascii_lowercase().starts_with("digest"))
            .map(String::from);
        if let (401, Some(challenge)) = (response.status().as_u16(), challenge) {
            let url = retry.url();
            let uri = match url.query() {
                Some(q) => format!("{}?{q}", url.path()),
                None => url.path().to_string(),
            };
            let cnonce = format!("{:016x}", rand_u64());
            let value = digest_authorization(
                &challenge,
                &resolve(username, vars),
                &resolve(password, vars),
                retry.method().as_str(),
                &uri,
                &cnonce,
            );
            retry
                .headers_mut()
                .insert(reqwest::header::AUTHORIZATION, value.parse().map_err(|_| "Invalid digest header")?);
            response = client.execute(retry).map_err(describe_error)?;
        }
    }
    let status = response.status();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let header_bytes: usize = headers.iter().map(|(k, v)| k.len() + v.len() + 4).sum();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut body = Vec::new();
    response
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    let elapsed_ms = start.elapsed().as_millis();
    let truncated = body.len() > MAX_BODY_BYTES;
    body.truncate(MAX_BODY_BYTES);
    let cookies = headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .filter_map(|(_, v)| parse_cookie(v))
        .collect();
    Ok(Response {
        status: status.as_u16(),
        reason: status.canonical_reason().unwrap_or_default().into(),
        elapsed_ms,
        size: header_bytes + body.len(),
        headers,
        cookies,
        content_type,
        body,
        truncated,
    })
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish()
}

/// Parses `key=value, key="value"` pairs from a Digest challenge.
fn challenge_params(challenge: &str) -> Vec<(String, String)> {
    let rest = challenge.trim_start()[6..].trim();
    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    while chars.peek().is_some() {
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        let value: String = if chars.peek() == Some(&'"') {
            chars.next();
            let v = chars.by_ref().take_while(|c| *c != '"').collect();
            chars.by_ref().take_while(|c| *c != ',').for_each(drop);
            v
        } else {
            chars.by_ref().take_while(|c| *c != ',').collect()
        };
        out.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    out
}

/// RFC 7616 Digest `Authorization` value for one challenge (MD5 or SHA-256, qop auth or none).
fn digest_authorization(challenge: &str, user: &str, pass: &str, method: &str, uri: &str, cnonce: &str) -> String {
    use sha2::Digest;
    let params = challenge_params(challenge);
    let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).unwrap_or_default();
    let (realm, nonce, opaque) = (get("realm"), get("nonce"), get("opaque"));
    let algorithm = if get("algorithm").is_empty() { "MD5" } else { get("algorithm") };
    let hash = |s: String| -> String {
        if algorithm.to_ascii_uppercase().starts_with("SHA-256") {
            hex::encode(sha2::Sha256::digest(s.as_bytes()))
        } else {
            hex::encode(md5::Md5::digest(s.as_bytes()))
        }
    };
    let mut ha1 = hash(format!("{user}:{realm}:{pass}"));
    if algorithm.to_ascii_lowercase().ends_with("-sess") {
        ha1 = hash(format!("{ha1}:{nonce}:{cnonce}"));
    }
    let ha2 = hash(format!("{method}:{uri}"));
    let qop = get("qop").split(',').map(str::trim).any(|q| q == "auth");
    let nc = "00000001";
    let response = if qop {
        hash(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
    } else {
        hash(format!("{ha1}:{nonce}:{ha2}"))
    };
    let mut out = format!(
        "Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", algorithm={algorithm}, response=\"{response}\""
    );
    if qop {
        out.push_str(&format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\""));
    }
    if !opaque.is_empty() {
        out.push_str(&format!(", opaque=\"{opaque}\""));
    }
    out
}

fn sign_aws(
    req: &mut reqwest::blocking::Request,
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
    session_token: &str,
) -> Result<(), String> {
    use sha2::Digest;
    // ponytail: streamed bodies (form-data files) are sent as UNSIGNED-PAYLOAD, which only S3 accepts.
    let payload = match req.body().map(|b| b.as_bytes()) {
        None => hex::encode(sha2::Sha256::digest(b"")),
        Some(Some(bytes)) => hex::encode(sha2::Sha256::digest(bytes)),
        Some(None) => "UNSIGNED-PAYLOAD".into(),
    };
    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let headers = req.headers_mut();
    let mut set = |k: &'static str, v: &str| -> Result<(), String> {
        headers.insert(k, v.parse().map_err(|_| format!("Invalid {k}"))?);
        Ok(())
    };
    set("x-amz-date", &amz_date)?;
    set("x-amz-content-sha256", &payload)?;
    if !session_token.is_empty() {
        set("x-amz-security-token", session_token)?;
    }
    let signed: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let auth = aws_authorization(
        req.method().as_str(),
        req.url(),
        &signed,
        &payload,
        access_key,
        secret_key,
        region,
        service,
        &amz_date,
    );
    req.headers_mut()
        .insert(reqwest::header::AUTHORIZATION, auth.parse().map_err(|_| "Invalid AWS signature")?);
    Ok(())
}

/// AWS Signature Version 4 `Authorization` value. `headers` must include x-amz-date; host is added.
#[allow(clippy::too_many_arguments)]
fn aws_authorization(
    method: &str,
    url: &url::Url,
    headers: &[(String, String)],
    payload_hash: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    use hmac::Mac;
    use sha2::Digest;
    let encode = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    let mut query: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (encode(&k), encode(&v))).collect();
    query.sort();
    let query = query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_string(),
    };
    let mut canon: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .chain([("host".to_string(), host)])
        .collect();
    canon.sort();
    canon.dedup_by(|a, b| a.0 == b.0);
    let signed_headers = canon.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!(
        "{method}\n{}\n{query}\n{}\n{signed_headers}\n{payload_hash}",
        if url.path().is_empty() { "/" } else { url.path() },
        canon.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>(),
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(sha2::Sha256::digest(canonical)));
    let hmac = |key: &[u8], data: &str| {
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes any key length");
        mac.update(data.as_bytes());
        mac.finalize().into_bytes().to_vec()
    };
    let key = [date, region, service, "aws4_request"]
        .iter()
        .fold(format!("AWS4{secret_key}").into_bytes(), |key, part| hmac(&key, part));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={}",
        hex::encode(hmac(&key, &to_sign))
    )
}

fn describe_error(error: reqwest::Error) -> String {
    use std::error::Error;
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{KeyValue, RawLanguage};
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// One-shot HTTP server: returns the raw request it received.
    fn serve(reply: &'static [u8]) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            tx.send(read_request(&mut stream)).unwrap();
            stream.write_all(reply).unwrap();
        });
        (port, rx)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let n = stream.read(&mut chunk).unwrap();
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if n == 0 || bytes.len() >= end + 4 + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn sends_resolved_request_with_auth_and_reads_response() {
        let (port, rx) = serve(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nSet-Cookie: sid=abc; Path=/; HttpOnly\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}");
        let mut request = Request::default();
        request.method = "POST".into();
        request.set_url("{{base}}/users?q={{name}}".into());
        request.headers.push(KeyValue::new("X-Trace", "{{name}}"));
        request.auth = Auth::Bearer {
            token: "{{token}}".into(),
        };
        request.body_mode = BodyMode::Raw;
        request.raw_language = RawLanguage::Json;
        request.body_raw = "{\"user\":\"{{name}}\"}".into();
        let vars = vec![
            ("base".to_string(), format!("127.0.0.1:{port}")),
            ("token".into(), "secret".into()),
            ("name".into(), "Ada".into()),
        ];
        let response = execute(&request, &vars, &Settings::default()).unwrap();
        let sent = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(sent.starts_with("POST /users?q=Ada HTTP/1.1"), "{sent}");
        let lower = sent.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer secret"));
        assert!(lower.contains("x-trace: ada"));
        assert!(lower.contains("content-type: application/json"));
        assert!(lower.contains("user-agent: flokiruntime/"));
        assert!(sent.ends_with("{\"user\":\"Ada\"}"));
        assert_eq!((response.status, response.reason.as_str()), (201, "Created"));
        assert_eq!(response.pretty(), "{\n  \"ok\": true\n}");
        assert_eq!(response.cookies[0].name, "sid");
        assert!(response.cookies[0].http_only);
        assert_eq!(response.language(), "json");
    }

    #[test]
    fn sends_urlencoded_form_and_reports_bad_urls() {
        let (port, rx) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let mut request = Request::default();
        request.method = "POST".into();
        request.url = format!("http://127.0.0.1:{port}/f");
        request.body_mode = BodyMode::UrlEncoded;
        request.body_form = vec![KeyValue::new("a b", "1&2"), KeyValue {
            enabled: false,
            ..KeyValue::new("off", "x")
        }];
        execute(&request, &[], &Settings::default()).unwrap();
        let sent = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(sent.ends_with("a+b=1%262"), "{sent}");

        request.url = "ftp://x".into();
        assert!(execute(&request, &[], &Settings::default()).unwrap_err().contains("Unsupported protocol"));
        request.url = String::new();
        assert!(execute(&request, &[], &Settings::default()).is_err());
    }

    #[test]
    fn sends_multipart_files_and_graphql() {
        let dir = std::env::temp_dir().join(format!("floki-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, "file body").unwrap();
        let (port, rx) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let mut request = Request::default();
        request.method = "POST".into();
        request.url = format!("http://127.0.0.1:{port}/upload");
        request.body_mode = BodyMode::FormData;
        request.body_multipart = vec![
            KeyValue::new("name", "{{who}}"),
            KeyValue { file: true, ..KeyValue::new("doc", path.to_string_lossy()) },
        ];
        execute(&request, &[("who".into(), "Ada".into())], &Settings::default()).unwrap();
        let sent = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(sent.to_ascii_lowercase().contains("content-type: multipart/form-data; boundary="), "{sent}");
        assert!(sent.contains("name=\"name\"\r\n\r\nAda"), "{sent}");
        assert!(sent.contains("filename=\"note.txt\""), "{sent}");
        assert!(sent.contains("file body"), "{sent}");

        request.body_multipart[1].value = dir.join("missing").to_string_lossy().into();
        assert!(execute(&request, &[], &Settings::default()).unwrap_err().contains("Could not read file"));

        let (port, rx) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        request.url = format!("http://127.0.0.1:{port}/graphql");
        request.body_mode = BodyMode::GraphQl;
        request.graphql_query = "query { user(id: $id) { name } }".into();
        request.graphql_variables = "{\"id\": {{id}}}".into();
        execute(&request, &[("id".into(), "7".into())], &Settings::default()).unwrap();
        let sent = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(sent.to_ascii_lowercase().contains("content-type: application/json"));
        let body: serde_json::Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["variables"]["id"], 7);
        assert_eq!(body["query"], "query { user(id: $id) { name } }");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.00 KB");
    }

    #[test]
    fn digest_matches_rfc_2617_example() {
        let challenge = r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
        let header = digest_authorization(challenge, "Mufasa", "Circle Of Life", "GET", "/dir/index.html", "0a4f113b");
        assert!(header.contains(r#"response="6629fae49393a05397450978507c4ef1""#), "{header}");
        assert!(header.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#));
    }

    #[test]
    fn aws_signature_matches_aws_test_suite() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sign = |url: &str| {
            let url = url::Url::parse(url).unwrap();
            let headers = [("X-Amz-Date".to_string(), "20150830T123600Z".to_string())];
            aws_authorization("GET", &url, &headers, empty, "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "us-east-1", "service", "20150830T123600Z")
        };
        assert!(sign("https://example.amazonaws.com/")
            .ends_with("SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"));
        assert!(sign("https://example.amazonaws.com/?Param2=value2&Param1=value1")
            .ends_with("Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"));
    }

    #[test]
    fn digest_auth_retries_with_challenge_and_sends_binary_file() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for reply in [
                &b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"r\", nonce=\"n\", qop=\"auth\"\r\nContent-Length: 0\r\n\r\n"[..],
                &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"[..],
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                tx.send(read_request(&mut stream)).unwrap();
                stream.write_all(reply).unwrap();
            }
        });
        let file = std::env::temp_dir().join(format!("floki-binary-{port}.bin"));
        std::fs::write(&file, b"RAWBYTES").unwrap();
        let mut request = Request::default();
        request.method = "PUT".into();
        request.url = format!("http://127.0.0.1:{port}/upload");
        request.auth = Auth::Digest { username: "u".into(), password: "p".into() };
        request.body_mode = BodyMode::Binary;
        request.body_file = file.to_string_lossy().into_owned();
        let response = execute(&request, &[], &Settings::default()).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(response.status, 200);
        let (first, second) = (rx.recv().unwrap(), rx.recv().unwrap());
        assert!(!first.to_ascii_lowercase().contains("authorization:"));
        assert!(second.contains("Digest username=\"u\""), "{second}");
        assert!(second.contains("uri=\"/upload\""));
        assert!(second.ends_with("RAWBYTES"));
    }
}
