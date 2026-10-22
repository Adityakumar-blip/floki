//! Client for the Floki sync server (`server/`). Blocking; call it off the UI thread.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8787";

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub email: String,
    pub name: String,
    #[serde(default)]
    pub verified: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Session {
    pub token: String,
    pub user: User,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub role: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Me {
    pub teams: Vec<Team>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub team_name: Option<String>,
    pub role: String,
    pub version: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceData {
    pub version: i64,
    pub data: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Member {
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub role: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Invite {
    pub email: String,
    pub role: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Members {
    pub members: Vec<Member>,
    pub invites: Vec<Invite>,
}

pub enum PushError {
    /// Someone else pushed first.
    Conflict,
    /// The user may no longer edit, or no longer see, the workspace.
    Forbidden,
    Other(String),
}

pub struct Client {
    server: String,
    token: Option<String>,
}

impl Client {
    pub fn new(server: &str, token: Option<&str>) -> Self {
        Self { server: server.trim_end_matches('/').to_string(), token: token.map(String::from) }
    }

    fn call<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<Value>) -> Result<T, (u16, String)> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| (0, e.to_string()))?;
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("valid method");
        let mut req = client.request(method, format!("{}/api{path}", self.server));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.header("Content-Type", "application/json").body(body.to_string());
        }
        let res = req.send().map_err(|e| (0, format!("Could not reach the server at {}: {e}", self.server)))?;
        let status = res.status().as_u16();
        let text = res.text().map_err(|e| (status, e.to_string()))?;
        if !(200..300).contains(&status) {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error")?.as_str().map(String::from))
                .unwrap_or_else(|| format!("Server returned {status}"));
            return Err((status, message));
        }
        // 204 responses have no body; `()` deserializes from null.
        serde_json::from_str(if text.trim().is_empty() { "null" } else { &text }).map_err(|e| (status, e.to_string()))
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        self.call("GET", path, None).map_err(|e| e.1)
    }

    fn send<T: DeserializeOwned>(&self, method: &str, path: &str, body: Value) -> Result<T, String> {
        self.call(method, path, Some(body)).map_err(|e| e.1)
    }

    pub fn signup(&self, name: &str, email: &str, password: &str) -> Result<Session, String> {
        self.send("POST", "/auth/signup", json!({"name": name, "email": email, "password": password}))
    }

    pub fn login(&self, email: &str, password: &str) -> Result<Session, String> {
        self.send("POST", "/auth/login", json!({"email": email, "password": password}))
    }

    pub fn logout(&self) -> Result<(), String> {
        self.send("POST", "/auth/logout", json!({}))
    }

    pub fn verify(&self, code: &str) -> Result<(), String> {
        self.send::<Value>("POST", "/auth/verify", json!({"code": code})).map(|_| ())
    }

    pub fn resend_code(&self) -> Result<(), String> {
        self.send("POST", "/auth/verify/resend", json!({}))
    }

    pub fn me(&self) -> Result<Me, String> {
        self.get("/me")
    }

    pub fn create_team(&self, name: &str) -> Result<Team, String> {
        self.send("POST", "/teams", json!({"name": name}))
    }

    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, String> {
        self.get("/workspaces")
    }

    pub fn create_workspace(&self, name: &str, team: Option<&str>, data: &impl Serialize) -> Result<WorkspaceInfo, String> {
        self.send("POST", "/workspaces", json!({"name": name, "team_id": team, "data": data}))
    }

    pub fn workspace(&self, id: &str) -> Result<WorkspaceData, String> {
        self.get(&format!("/workspaces/{id}"))
    }

    pub fn push(&self, id: &str, base_version: i64, data: &impl Serialize) -> Result<i64, PushError> {
        match self.call::<Value>("PUT", &format!("/workspaces/{id}/data"), Some(json!({"base_version": base_version, "data": data}))) {
            Ok(v) => Ok(v["version"].as_i64().unwrap_or_default()),
            Err((409, _)) => Err(PushError::Conflict),
            Err((403 | 404, _)) => Err(PushError::Forbidden),
            Err((_, e)) => Err(PushError::Other(e)),
        }
    }

    pub fn rename_workspace(&self, id: &str, name: &str) -> Result<(), String> {
        self.send("PATCH", &format!("/workspaces/{id}"), json!({"name": name}))
    }

    pub fn delete_workspace(&self, id: &str) -> Result<(), String> {
        self.call("DELETE", &format!("/workspaces/{id}"), None).map_err(|e| e.1)
    }

    /// `scope` is `teams` or `workspaces`.
    pub fn members(&self, scope: &str, id: &str) -> Result<Members, String> {
        self.get(&format!("/{scope}/{id}/members"))
    }

    /// Adds an existing user or leaves an invite for when they sign up; returns `added` or `invited`.
    pub fn add_member(&self, scope: &str, id: &str, email: &str, role: &str) -> Result<String, String> {
        self.send::<Value>("POST", &format!("/{scope}/{id}/members"), json!({"email": email, "role": role}))
            .map(|v| v["status"].as_str().unwrap_or("added").to_string())
    }

    pub fn set_role(&self, scope: &str, id: &str, user: &str, role: &str) -> Result<(), String> {
        self.send("PATCH", &format!("/{scope}/{id}/members/{user}"), json!({"role": role}))
    }

    pub fn remove_member(&self, scope: &str, id: &str, user: &str) -> Result<(), String> {
        self.call("DELETE", &format!("/{scope}/{id}/members/{user}"), None).map_err(|e| e.1)
    }

    pub fn cancel_invite(&self, scope: &str, id: &str, email: &str) -> Result<(), String> {
        let email = url::form_urlencoded::byte_serialize(email.as_bytes()).collect::<String>();
        self.call("DELETE", &format!("/{scope}/{id}/invites/{email}"), None).map_err(|e| e.1)
    }
}

/// A Google sign-in in progress: the browser is sent to `url` and Google redirects back to `listener`.
pub struct GoogleFlow {
    pub url: String,
    listener: TcpListener,
    verifier: String,
    state: String,
    redirect: String,
}

fn random_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Starts Google's loopback + PKCE flow for desktop apps; the server holds the client secret.
pub fn google_start(server: &str) -> Result<GoogleFlow, String> {
    use base64::Engine;
    use sha2::Digest;
    let config: Value = Client::new(server, None).get("/auth/google/config")?;
    let client_id = config["client_id"].as_str().ok_or("Google sign-in is not configured on this server")?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());
    let (verifier, state) = (random_token(), random_token());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let mut url = url::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").expect("valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email profile")
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("prompt", "select_account");
    Ok(GoogleFlow { url: url.into(), listener, verifier, state, redirect })
}

/// Waits up to three minutes for Google's redirect, then trades the code for a Floki session.
pub fn google_finish(server: &str, flow: GoogleFlow) -> Result<Session, String> {
    flow.listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let (mut stream, _) = loop {
        match flow.listener.accept() {
            Ok(conn) => break conn,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Err(_) => return Err("Google sign-in timed out".into()),
        }
    };
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let target = head.split_whitespace().nth(1).unwrap_or("/");
    let query = url::Url::parse(&format!("http://localhost{target}")).map_err(|e| e.to_string())?;
    let param = |k: &str| query.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    let page = |msg: &str| {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><title>Floki</title><body style=\"font-family:sans-serif;padding:40px\"><h2>{msg}</h2><p>You can close this tab and return to Floki.</p></body>")
    };
    if param("state").as_deref() != Some(flow.state.as_str()) {
        let _ = stream.write_all(page("Sign-in failed").as_bytes());
        return Err("Google sign-in failed: state mismatch".into());
    }
    let Some(code) = param("code") else {
        let _ = stream.write_all(page("Sign-in was cancelled").as_bytes());
        return Err(format!("Google sign-in cancelled: {}", param("error").unwrap_or_default()));
    };
    let result: Result<Session, String> = Client::new(server, None).send(
        "POST",
        "/auth/google",
        json!({"code": code, "code_verifier": flow.verifier, "redirect_uri": flow.redirect}),
    );
    let _ = stream.write_all(page(if result.is_ok() { "Signed in to Floki" } else { "Sign-in failed" }).as_bytes());
    result
}
