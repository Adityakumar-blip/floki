use crate::model::{graphql_body, resolve, Auth, BodyMode, Example, OAuth2, Request, Settings, StoredCookie};
use reqwest::header::HeaderValue;
use std::io::Read;
use std::sync::{Arc, Mutex};
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
    /// Server-sent events, when the response was `text/event-stream`.
    pub events: Vec<SseEvent>,
}

/// One server-sent event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SseEvent {
    /// `message` unless the server named it.
    pub event: String,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
    /// Milliseconds since the request was sent.
    pub at_ms: u128,
}

/// Incremental `text/event-stream` parser (WHATWG HTML, "Server-sent events"): feed it chunks as they arrive.
#[derive(Default)]
pub struct SseParser {
    line: Vec<u8>,
    /// A chunk ended on `\r`, so a `\n` starting the next one belongs to the same line break.
    after_cr: bool,
    event: String,
    data: String,
    has_data: bool,
    id: Option<String>,
    retry: Option<u64>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &[u8], at_ms: u128) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &b in chunk {
            if std::mem::take(&mut self.after_cr) && b == b'\n' {
                continue;
            }
            match b {
                b'\n' | b'\r' => {
                    self.after_cr = b == b'\r';
                    let line = std::mem::take(&mut self.line);
                    out.extend(self.line_done(&String::from_utf8_lossy(&line), at_ms));
                }
                _ => self.line.push(b),
            }
        }
        out
    }

    /// The stream ended; a last event without its blank line is still delivered.
    pub fn finish(&mut self, at_ms: u128) -> Vec<SseEvent> {
        let mut out = self.feed(b"\n", at_ms);
        out.extend(self.line_done("", at_ms));
        out
    }

    fn line_done(&mut self, line: &str, at_ms: u128) -> Option<SseEvent> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            let data = std::mem::take(&mut self.data);
            if !std::mem::take(&mut self.has_data) {
                return None;
            }
            return Some(SseEvent {
                event: if event.is_empty() { "message".into() } else { event },
                data: data.strip_suffix('\n').unwrap_or(&data).to_string(),
                id: self.id.clone(),
                retry: self.retry,
                at_ms,
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
        match field {
            "event" => self.event = value.into(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.id = Some(value.into()),
            "retry" => self.retry = value.parse().ok().or(self.retry),
            _ => {}
        }
        None
    }
}

/// The answer an AI API streamed, joined from its events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Assembled {
    /// `OpenAI`, `Anthropic` or `Gemini` (the format, which other providers copy).
    pub format: &'static str,
    pub text: String,
    /// Reasoning or thinking text, when the stream had it.
    pub thinking: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub finish_reason: Option<String>,
}

/// Joins streamed text from OpenAI-style (Chat Completions and Responses), Anthropic and Gemini events; `None` for other streams.
pub fn assemble(events: &[SseEvent]) -> Option<Assembled> {
    let mut a = Assembled::default();
    let str_at = |v: &serde_json::Value, path: &str| v.pointer(path).and_then(|x| x.as_str()).map(String::from);
    let num_at = |v: &serde_json::Value, path: &str| v.pointer(path).and_then(|x| x.as_u64());
    for e in events {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&e.data) else { continue };
        let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or(&e.event);
        if let Some(choices) = v.get("choices").and_then(|c| c.as_array()) {
            a.format = "OpenAI";
            for c in choices {
                a.text.push_str(&str_at(c, "/delta/content").or_else(|| str_at(c, "/text")).unwrap_or_default());
                a.thinking.push_str(&str_at(c, "/delta/reasoning_content").or_else(|| str_at(c, "/delta/reasoning")).unwrap_or_default());
                a.finish_reason = str_at(c, "/finish_reason").or(a.finish_reason.take());
            }
            a.input_tokens = num_at(&v, "/usage/prompt_tokens").or(a.input_tokens);
            a.output_tokens = num_at(&v, "/usage/completion_tokens").or(a.output_tokens);
        } else if kind.starts_with("response.") {
            a.format = "OpenAI";
            match kind {
                "response.output_text.delta" => a.text.push_str(&str_at(&v, "/delta").unwrap_or_default()),
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    a.thinking.push_str(&str_at(&v, "/delta").unwrap_or_default())
                }
                "response.completed" | "response.incomplete" => {
                    a.input_tokens = num_at(&v, "/response/usage/input_tokens");
                    a.output_tokens = num_at(&v, "/response/usage/output_tokens");
                    a.finish_reason = str_at(&v, "/response/status");
                }
                _ => {}
            }
        } else if matches!(kind, "message_start" | "content_block_delta" | "message_delta" | "content_block_start") {
            a.format = "Anthropic";
            match kind {
                "message_start" => a.input_tokens = num_at(&v, "/message/usage/input_tokens"),
                "content_block_delta" => {
                    a.text.push_str(&str_at(&v, "/delta/text").unwrap_or_default());
                    a.thinking.push_str(&str_at(&v, "/delta/thinking").unwrap_or_default());
                }
                "message_delta" => {
                    a.output_tokens = num_at(&v, "/usage/output_tokens").or(a.output_tokens);
                    a.finish_reason = str_at(&v, "/delta/stop_reason").or(a.finish_reason.take());
                }
                _ => {}
            }
        } else if let Some(candidates) = v.get("candidates").and_then(|c| c.as_array()) {
            a.format = "Gemini";
            for part in candidates.first().and_then(|c| c.pointer("/content/parts")).and_then(|p| p.as_array()).into_iter().flatten() {
                let text = part.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                if part.get("thought").and_then(|t| t.as_bool()) == Some(true) {
                    a.thinking.push_str(text);
                } else {
                    a.text.push_str(text);
                }
            }
            a.finish_reason = candidates.first().and_then(|c| str_at(c, "/finishReason")).or(a.finish_reason.take());
            a.input_tokens = num_at(&v, "/usageMetadata/promptTokenCount").or(a.input_tokens);
            a.output_tokens = num_at(&v, "/usageMetadata/candidatesTokenCount").or(a.output_tokens);
        }
    }
    (!a.format.is_empty()).then_some(a)
}

/// Called with each event as it arrives; returning false stops reading the stream.
pub type OnEvent<'a> = &'a mut dyn FnMut(&SseEvent) -> bool;

/// Most events an SSE response keeps; the log and scripts don't need more.
const MAX_EVENTS: usize = 100_000;

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

    pub fn is_html(&self) -> bool {
        let start = self.body.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(0);
        let head = String::from_utf8_lossy(&self.body[start..self.body.len().min(start + 15)]).to_ascii_lowercase();
        self.content_type.contains("html") || head.starts_with("<!doctype html") || head.starts_with("<html")
    }

    /// The image type for an `image/*` response, by its MIME subtype.
    pub fn image_type(&self) -> Option<&'static str> {
        let subtype = self.content_type.strip_prefix("image/")?.split(';').next()?.trim();
        Some(match subtype {
            "png" => "png",
            "jpeg" | "jpg" => "jpeg",
            "gif" => "gif",
            "webp" => "webp",
            "svg+xml" => "svg",
            "bmp" => "bmp",
            "tiff" => "tiff",
            _ => return None,
        })
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
            events: Vec::new(),
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

/// Cookie jar shared by the requests of a tab or a run; reqwest feeds it every Set-Cookie, redirects included.
#[derive(Debug, Default)]
pub struct CookieJar(Mutex<Vec<StoredCookie>>);

impl CookieJar {
    pub fn new(cookies: Vec<StoredCookie>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(cookies)))
    }

    /// Unexpired cookies, for saving back to the workspace.
    pub fn cookies(&self) -> Vec<StoredCookie> {
        let now = chrono::Utc::now().timestamp();
        self.0.lock().map(|c| c.iter().filter(|c| c.expires.is_none_or(|t| t > now)).cloned().collect()).unwrap_or_default()
    }

    /// Stores a Set-Cookie header received from `url`. Returns false for a cookie the host may not set.
    pub fn store(&self, header: &str, url: &url::Url) -> bool {
        let Some(cookie) = cookie_from_header(header, url) else { return false };
        let Ok(mut jar) = self.0.lock() else { return false };
        jar.retain(|c| !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path));
        if cookie.expires.is_none_or(|t| t > chrono::Utc::now().timestamp()) {
            jar.push(cookie);
        }
        true
    }

    /// The Cookie header value for a request to `url`.
    pub fn header_for(&self, url: &url::Url) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let now = chrono::Utc::now().timestamp();
        let jar = self.0.lock().ok()?;
        let mut matching: Vec<&StoredCookie> = jar
            .iter()
            .filter(|c| {
                let domain_ok = if c.host_only { host == c.domain } else { domain_matches(&host, &c.domain) };
                domain_ok
                    && path_matches(url.path(), &c.path)
                    && (!c.secure || url.scheme() == "https")
                    && c.expires.is_none_or(|t| t > now)
            })
            .collect();
        // Longer paths first, as browsers do.
        matching.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        (!matching.is_empty()).then(|| matching.iter().map(|c| format!("{}={}", c.name, c.value)).collect::<Vec<_>>().join("; "))
    }
}

impl reqwest::cookie::CookieStore for CookieJar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &url::Url) {
        for header in headers.filter_map(|h| h.to_str().ok()) {
            self.store(header, url);
        }
    }

    fn cookies(&self, url: &url::Url) -> Option<HeaderValue> {
        self.header_for(url).and_then(|h| HeaderValue::from_str(&h).ok())
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'))
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || request_path.strip_prefix(cookie_path).is_some_and(|rest| cookie_path.ends_with('/') || rest.starts_with('/'))
}

fn cookie_from_header(header: &str, url: &url::Url) -> Option<StoredCookie> {
    let host = url.host_str()?.to_ascii_lowercase();
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let default_path = match url.path().rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => url.path()[..i].to_string(),
    };
    let mut cookie = StoredCookie {
        name: name.trim().into(),
        value: value.trim().trim_matches('"').into(),
        domain: host.clone(),
        path: default_path,
        expires: None,
        host_only: true,
        secure: false,
        http_only: false,
    };
    if cookie.name.is_empty() {
        return None;
    }
    let mut max_age = None;
    for attr in parts {
        let (k, v) = attr.split_once('=').map_or((attr.trim(), ""), |(k, v)| (k.trim(), v.trim()));
        match k.to_ascii_lowercase().as_str() {
            "domain" if !v.is_empty() => {
                let domain = v.trim_start_matches('.').to_ascii_lowercase();
                if !domain_matches(&host, &domain) {
                    return None;
                }
                cookie.domain = domain;
                cookie.host_only = false;
            }
            "path" if v.starts_with('/') => cookie.path = v.into(),
            "expires" => {
                cookie.expires = chrono::DateTime::parse_from_rfc2822(v)
                    .or_else(|_| chrono::DateTime::parse_from_str(&v.replace(" GMT", " +0000"), "%a, %d-%b-%Y %H:%M:%S %z"))
                    .ok()
                    .map(|t| t.timestamp())
                    .or(cookie.expires)
            }
            "max-age" => max_age = v.parse::<i64>().ok(),
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            _ => {}
        }
    }
    if let Some(age) = max_age {
        cookie.expires = Some(chrono::Utc::now().timestamp() + age);
    }
    Some(cookie)
}

fn has_header(request: &Request, name: &str) -> bool {
    request
        .headers
        .iter()
        .any(|h| h.enabled && h.key.trim().eq_ignore_ascii_case(name))
}

fn client(request: &Request, settings: &Settings, jar: Option<Arc<CookieJar>>) -> Result<reqwest::blocking::Client, String> {
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
    if let Some(jar) = jar {
        builder = builder.cookie_provider(jar);
    }
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
    let mut form = vec![("grant_type", o.grant_type.clone())];
    if o.grant_type == "password" {
        form.push(("username", r(&o.username)));
        form.push(("password", r(&o.password)));
    }
    if !o.scope.trim().is_empty() {
        form.push(("scope", r(&o.scope)));
    }
    request_token(o, vars, settings, form)
}

/// Posts `form` to the token URL with the client credentials and returns `access_token`.
pub fn request_token(o: &OAuth2, vars: &[(String, String)], settings: &Settings, mut form: Vec<(&str, String)>) -> Result<String, String> {
    let r = |s: &str| resolve(s.trim(), vars);
    if o.token_url.trim().is_empty() {
        return Err("Enter an Access Token URL".into());
    }
    let mut builder = client(&Request::default(), settings, None)?.post(r(&o.token_url)).header("Accept", "application/json");
    if o.client_auth == "body" {
        form.push(("client_id", r(&o.client_id)));
        if !o.client_secret.trim().is_empty() {
            form.push(("client_secret", r(&o.client_secret)));
        }
    } else {
        builder = builder.basic_auth(r(&o.client_id), Some(r(&o.client_secret)));
    }
    let response = builder.form(&form).send().map_err(describe_error)?;
    let status = response.status();
    let text = response.text().map_err(describe_error)?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("access_token")?.as_str().map(String::from))
        // Some servers (GitHub) answer in form encoding unless asked otherwise.
        .or_else(|| url::form_urlencoded::parse(text.as_bytes()).find(|(k, _)| k == "access_token").map(|(_, v)| v.into_owned()))
        .ok_or_else(|| format!("Token request failed ({status}): {text}"))
}

/// Sends `request` with `{{variables}}` resolved. Blocking; call it off the UI thread.
pub fn execute(request: &Request, vars: &[(String, String)], settings: &Settings) -> Result<Response, String> {
    execute_with(request, vars, settings, None)
}

/// `execute` that also sends cookies from `jar` and stores the ones the server sets.
pub fn execute_with(request: &Request, vars: &[(String, String)], settings: &Settings, jar: Option<Arc<CookieJar>>) -> Result<Response, String> {
    execute_stream(request, vars, settings, jar, &mut |_| true)
}

/// `execute_with` that reports server-sent events through `on_event` while a `text/event-stream` response is still arriving.
pub fn execute_stream(
    request: &Request,
    vars: &[(String, String)],
    settings: &Settings,
    jar: Option<Arc<CookieJar>>,
    on_event: OnEvent,
) -> Result<Response, String> {
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
    let jwt = match &request.auth {
        Auth::Jwt(j) => Some(crate::auth::jwt(j, vars)?),
        _ => None,
    };
    if let (Auth::Jwt(j), Some(token)) = (&request.auth, &jwt) {
        if j.add_to == "query" {
            parsed.query_pairs_mut().append_pair(&resolve(j.query_key.trim(), vars), token);
        }
    }
    let method = reqwest::Method::from_bytes(request.method.trim().to_uppercase().as_bytes())
        .map_err(|_| format!("Invalid method \"{}\"", request.method))?;
    let client = client(request, settings, jar)?;
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
        Auth::Jwt(j) if j.add_to != "query" => {
            let token = jwt.as_deref().unwrap_or_default();
            builder = builder.header("Authorization", format!("{} {token}", resolve(j.header_prefix.trim(), vars)).trim());
        }
        Auth::Ntlm(_) => {
            builder = builder.header("Authorization", format!("NTLM {}", crate::auth::ntlm_negotiate()));
        }
        Auth::ApiKey { .. }
        | Auth::OAuth2(_)
        | Auth::Digest { .. }
        | Auth::AwsV4 { .. }
        | Auth::Jwt(_)
        | Auth::OAuth1(_)
        | Auth::Hawk(_) => {}
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
    let now = chrono::Utc::now().timestamp();
    match &request.auth {
        Auth::OAuth1(o) => {
            // Form fields are part of the signature.
            let is_form = outgoing.headers().get(reqwest::header::CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"application/x-www-form-urlencoded"));
            let form: Vec<(String, String)> = match outgoing.body().and_then(|b| b.as_bytes()) {
                Some(body) if is_form => url::form_urlencoded::parse(body).map(|(k, v)| (k.into_owned(), v.into_owned())).collect(),
                _ => Vec::new(),
            };
            let method = outgoing.method().to_string();
            let params = crate::auth::oauth1_params(o, vars, &method, outgoing.url(), &form, &crate::auth::oauth1_nonce(), now);
            if o.add_to == "query" {
                outgoing.url_mut().query_pairs_mut().extend_pairs(params.iter());
            } else {
                let header = crate::auth::oauth1_header(o, vars, &params);
                outgoing.headers_mut().insert(reqwest::header::AUTHORIZATION, header.parse().map_err(|_| "Invalid OAuth 1.0 header")?);
            }
        }
        Auth::Hawk(h) => {
            let header = crate::auth::hawk_header(h, vars, outgoing.method().as_str(), outgoing.url(), &crate::auth::hawk_nonce(), now);
            outgoing.headers_mut().insert(reqwest::header::AUTHORIZATION, header.parse().map_err(|_| "Invalid Hawk header")?);
        }
        _ => {}
    }
    // Digest and NTLM need the server's challenge, so the first attempt is expected to get a 401.
    let retry = matches!(request.auth, Auth::Digest { .. } | Auth::Ntlm(_)).then(|| outgoing.try_clone()).flatten();
    let start = Instant::now();
    let mut response = client.execute(outgoing).map_err(describe_error)?;
    match (retry, &request.auth) {
        (Some(mut retry), Auth::Digest { username, password }) => {
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
        (Some(mut retry), Auth::Ntlm(n)) => {
            let challenge = response
                .headers()
                .get_all(reqwest::header::WWW_AUTHENTICATE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(|v| v.trim().strip_prefix("NTLM ").map(String::from));
            if let (401, Some(challenge)) = (response.status().as_u16(), challenge) {
                // NTLM authenticates the connection, so the body is drained to keep it open for the second leg.
                let _ = response.bytes();
                let message = crate::auth::ntlm_authenticate(n, vars, &challenge)?;
                retry
                    .headers_mut()
                    .insert(reqwest::header::AUTHORIZATION, format!("NTLM {message}").parse().map_err(|_| "Invalid NTLM header")?);
                response = client.execute(retry).map_err(describe_error)?;
            }
        }
        _ => {}
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
    let mut events = Vec::new();
    let mut truncated = false;
    if content_type.starts_with("text/event-stream") {
        let mut parser = SseParser::default();
        let mut chunk = [0_u8; 8192];
        let mut deliver = |batch: Vec<SseEvent>, events: &mut Vec<SseEvent>| {
            let mut go_on = true;
            for event in batch {
                go_on &= on_event(&event);
                if events.len() < MAX_EVENTS {
                    events.push(event);
                }
            }
            go_on
        };
        loop {
            let n = match response.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                // A stream that drops midway still shows what arrived.
                Err(e) if !events.is_empty() || !body.is_empty() => {
                    events.push(SseEvent { event: "error".into(), data: format!("Stream ended: {e}"), at_ms: start.elapsed().as_millis(), ..SseEvent::default() });
                    break;
                }
                Err(e) => return Err(e.to_string()),
            };
            if body.len() < MAX_BODY_BYTES {
                body.extend_from_slice(&chunk[..n.min(MAX_BODY_BYTES - body.len())]);
            } else {
                truncated = true;
            }
            if !deliver(parser.feed(&chunk[..n], start.elapsed().as_millis()), &mut events) {
                break;
            }
        }
        deliver(parser.finish(start.elapsed().as_millis()), &mut events);
    } else {
        response.take(MAX_BODY_BYTES as u64 + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
        truncated = body.len() > MAX_BODY_BYTES;
        body.truncate(MAX_BODY_BYTES);
    }
    let elapsed_ms = start.elapsed().as_millis();
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
        events,
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
mod sse_tests {
    use super::*;

    fn parse(chunks: &[&str]) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        let mut out: Vec<SseEvent> = chunks.iter().flat_map(|c| p.feed(c.as_bytes(), 0)).collect();
        out.extend(p.finish(0));
        out
    }

    #[test]
    fn parses_the_event_stream_format() {
        let events = parse(&[
            ": keep-alive comment\n\nevent: update\nid: 7\nretry: 3000\nda",
            "ta: {\"a\":\r\ndata:1}\r\n\r\ndata:no space\r\rdata: trailing",
        ]);
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!((events[0].event.as_str(), events[0].data.as_str()), ("update", "{\"a\":\n1}"));
        assert_eq!((events[0].id.as_deref(), events[0].retry), (Some("7"), Some(3000)));
        assert_eq!((events[1].event.as_str(), events[1].data.as_str()), ("message", "no space"), "event type resets; id carries on");
        assert_eq!(events[1].id.as_deref(), Some("7"));
        assert_eq!(events[2].data, "trailing", "a last event without a blank line is kept");
        assert!(parse(&["event: x\n\n", "id: 1\n\n"]).is_empty(), "no data, no event");
        assert_eq!(parse(&["data: a\r", "\ndata: b\n\n"])[0].data, "a\nb", "CRLF split across chunks is one line break");
    }

    fn events(datas: &[&str]) -> Vec<SseEvent> {
        datas.iter().map(|d| SseEvent { event: "message".into(), data: d.to_string(), ..SseEvent::default() }).collect()
    }

    #[test]
    fn assembles_openai_anthropic_and_gemini_streams() {
        let openai = assemble(&events(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
            "[DONE]",
        ]))
        .unwrap();
        assert_eq!((openai.format, openai.text.as_str(), openai.finish_reason.as_deref()), ("OpenAI", "Hello", Some("stop")));
        assert_eq!((openai.input_tokens, openai.output_tokens), (Some(9), Some(2)));

        let responses = assemble(&events(&[
            r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
            r#"{"type":"response.output_text.delta","delta":"!"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":4,"output_tokens":2}}}"#,
        ]))
        .unwrap();
        assert_eq!((responses.text.as_str(), responses.output_tokens), ("Hi!", Some(2)));

        let anthropic = assemble(&events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me see."}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Paris"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ]))
        .unwrap();
        assert_eq!((anthropic.format, anthropic.text.as_str(), anthropic.thinking.as_str()), ("Anthropic", "Paris", "Let me see."));
        assert_eq!((anthropic.input_tokens, anthropic.output_tokens, anthropic.finish_reason.as_deref()), (Some(12), Some(5), Some("end_turn")));

        let gemini = assemble(&events(&[
            r#"{"candidates":[{"content":{"parts":[{"text":"Bon"}]}}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"jour"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#,
        ]))
        .unwrap();
        assert_eq!((gemini.format, gemini.text.as_str(), gemini.output_tokens), ("Gemini", "Bonjour", Some(2)));

        assert!(assemble(&events(&["{\"price\": 1}", "plain text"])).is_none(), "other streams have no assembled view");
    }

    /// Server that writes `chunks`, waiting for `gate` before each one after the first.
    fn stream_server(chunks: Vec<&'static str>, gate: std::sync::mpsc::Receiver<()>) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 4096];
            let _ = std::io::Read::read(&mut s, &mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n");
            for (i, chunk) in chunks.iter().enumerate() {
                if i > 0 && gate.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
                    return;
                }
                if s.write_all(chunk.as_bytes()).and_then(|_| s.flush()).is_err() {
                    return;
                }
            }
        });
        (port, handle)
    }

    #[test]
    fn events_arrive_while_the_stream_is_open() {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let (port, server) = stream_server(vec!["data: one\n\nda", "ta: two\n\n", "event: end\ndata: three\n\n"], gate_rx);
        let mut request = Request::default();
        request.url = format!("http://127.0.0.1:{port}/stream");
        let mut seen = Vec::new();
        // Each event must reach us before the server is allowed to send the next chunk, proving it isn't buffered.
        let response = execute_stream(&request, &[], &Settings::default(), None, &mut |e| {
            seen.push(e.data.clone());
            let _ = gate_tx.send(());
            let _ = gate_tx.send(());
            true
        })
        .unwrap();
        server.join().unwrap();
        assert_eq!(seen, ["one", "two", "three"]);
        assert_eq!(response.events.len(), 3);
        assert_eq!(response.events[2].event, "end");
        assert!(response.text().contains("data: two"), "the raw stream stays available");
    }

    #[test]
    fn returning_false_stops_an_endless_stream() {
        let (_gate_tx, gate_rx) = std::sync::mpsc::channel();
        let (port, _server) = stream_server(vec!["data: 1\n\ndata: 2\n\n", "data: never\n\n"], gate_rx);
        let mut request = Request::default();
        request.url = format!("http://127.0.0.1:{port}/");
        let response = execute_stream(&request, &[], &Settings::default(), None, &mut |_| false).unwrap();
        assert_eq!(response.events.len(), 2, "the chunk already read is kept, then reading stops");
    }
}

#[cfg(test)]
mod cookie_tests {
    use super::*;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn jar_follows_domain_path_secure_and_expiry_rules() {
        let jar = CookieJar::new(Vec::new());
        assert!(jar.store("a=1; Path=/", &url("https://api.example.com/v1/login")));
        assert!(jar.store("b=2; Domain=.example.com; Path=/v1; Secure; HttpOnly", &url("https://api.example.com/v1/login")));
        assert!(jar.store("c=3", &url("https://api.example.com/v1/login")), "default path is /v1");
        assert!(!jar.store("evil=1; Domain=other.com", &url("https://api.example.com/")), "a host can't set cookies for another domain");
        assert!(jar.store("gone=1; Max-Age=0", &url("https://api.example.com/")));
        assert!(jar.store("old=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", &url("https://api.example.com/")));

        assert_eq!(jar.header_for(&url("https://api.example.com/v1/users")).as_deref(), Some("b=2; c=3; a=1"));
        assert_eq!(jar.header_for(&url("https://www.example.com/v1")).as_deref(), Some("b=2"), "only Domain cookies reach subdomains");
        assert_eq!(jar.header_for(&url("http://api.example.com/v1")).as_deref(), Some("c=3; a=1"), "Secure needs https");
        assert_eq!(jar.header_for(&url("https://api.example.com/v10")).as_deref(), Some("a=1"), "/v1 does not match /v10");
        assert_eq!(jar.cookies().len(), 3);

        jar.store("a=updated; Path=/", &url("https://api.example.com/"));
        assert!(jar.header_for(&url("https://api.example.com/")).unwrap().contains("a=updated"));
        assert_eq!(jar.cookies().len(), 3, "same name, domain and path replaces");
    }

    #[test]
    fn ntlm_handshake_runs_on_one_connection() {
        use base64::Engine;
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // Only one connection is accepted, so the second leg must reuse it.
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut auth_headers = Vec::new();
            for reply in [
                {
                    let mut challenge = b"NTLMSSP\0\x02\0\0\0".to_vec();
                    challenge.extend([0_u8; 8]); // target name
                    challenge.extend(0x0008_8205_u32.to_le_bytes());
                    challenge.extend([1, 2, 3, 4, 5, 6, 7, 8]); // server challenge
                    challenge.extend([0_u8; 16]); // reserved and empty target info
                    let b64 = base64::engine::general_purpose::STANDARD.encode(challenge);
                    format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: NTLM {b64}\r\nContent-Length: 4\r\n\r\nnope")
                },
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_string(),
            ] {
                let mut line = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line.to_lowercase().starts_with("authorization:") {
                        auth_headers.push(line.trim().to_string());
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                writer.write_all(reply.as_bytes()).unwrap();
            }
            auth_headers
        });
        let mut request = Request::default();
        request.url = format!("http://127.0.0.1:{port}/secure");
        request.auth = Auth::Ntlm(crate::model::Ntlm { username: "CORP\\ada".into(), password: "pw".into(), ..Default::default() });
        let response = execute(&request, &[], &Settings::default()).unwrap();
        assert_eq!(response.status, 200);
        let headers = server.join().unwrap();
        let decode = |h: &str| base64::engine::general_purpose::STANDARD.decode(h.rsplit(' ').next().unwrap()).unwrap();
        assert_eq!(decode(&headers[0])[8], 1, "negotiate first");
        let auth = decode(&headers[1]);
        assert_eq!(auth[8], 3, "then authenticate");
        let utf16 = |s: &str| s.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        assert!(auth.windows(8).any(|w| w == utf16("CORP").as_slice()), "the domain comes from DOMAIN\\user");
    }

    #[test]
    fn redirect_cookies_are_kept_and_sent() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut second = String::new();
            for reply in [
                "HTTP/1.1 302 Found\r\nLocation: /home\r\nSet-Cookie: session=abc; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ] {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 4096];
                let n = s.read(&mut buf).unwrap();
                second = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                s.write_all(reply.as_bytes()).unwrap();
            }
            second
        });
        let mut request = Request::default();
        request.url = format!("http://127.0.0.1:{port}/login");
        let jar = CookieJar::new(Vec::new());
        let response = execute_with(&request, &[], &Settings::default(), Some(jar.clone())).unwrap();
        assert_eq!(response.status, 200);
        assert!(server.join().unwrap().contains("cookie: session=abc"));
        assert_eq!(jar.cookies()[0].value, "abc");
    }
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
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if bytes.len() >= end + 4 + len {
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
        let wait = || rx.recv_timeout(Duration::from_secs(5)).expect("mock server got no request");
        let (first, second) = (wait(), wait());
        assert!(!first.to_ascii_lowercase().contains("authorization:"));
        assert!(second.contains("Digest username=\"u\""), "{second}");
        assert!(second.contains("uri=\"/upload\""));
        assert!(second.ends_with("RAWBYTES"));
    }
}
