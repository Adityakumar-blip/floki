use crate::{err, ApiError, AppState, Res};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, HeaderMap, StatusCode},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use sqlx::PgConnection;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

pub fn norm_email(s: &str) -> Option<String> {
    let e = s.trim().to_lowercase();
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !e.contains(char::is_whitespace)
        && e.len() <= 254;
    ok.then_some(e)
}

fn hash_token(t: &str) -> String {
    B64.encode(Sha256::digest(t.as_bytes()))
}

pub fn is_unique(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

async fn hash_pw(pw: String) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>())?;
        Argon2::default().hash_password(pw.as_bytes(), &salt).map(|h| h.to_string())
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .ok_or_else(|| err(500, "Internal server error"))
}

async fn verify_pw(pw: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

fn bearer(h: &HeaderMap) -> Result<&str, ApiError> {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(|| err(401, "Missing bearer token"))
}

pub struct AuthUser {
    pub id: i64,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let token = bearer(&parts.headers)?;
        let id = sqlx::query_scalar::<_, i64>("SELECT user_id FROM sessions WHERE token_hash=$1")
            .bind(hash_token(token))
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| err(401, "Invalid or expired session"))?;
        Ok(AuthUser { id })
    }
}

async fn apply_invites(c: &mut PgConnection, uid: i64, email: &str) -> sqlx::Result<()> {
    for (members, invites, col) in [
        ("team_members", "team_invites", "team_id"),
        ("workspace_members", "workspace_invites", "workspace_id"),
    ] {
        sqlx::query(&format!(
            "INSERT INTO {members}({col},user_id,role) SELECT {col},$1,role FROM {invites} WHERE email=$2 ON CONFLICT DO NOTHING"
        ))
        .bind(uid)
        .bind(email)
        .execute(&mut *c)
        .await?;
        sqlx::query(&format!("DELETE FROM {invites} WHERE email=$1")).bind(email).execute(&mut *c).await?;
    }
    Ok(())
}

async fn user_json(c: &mut PgConnection, uid: i64) -> sqlx::Result<Value> {
    let (email, name, verified): (String, String, bool) =
        sqlx::query_as("SELECT email, COALESCE(name,email), email_verified FROM users WHERE id=$1")
            .bind(uid)
            .fetch_one(c)
            .await?;
    Ok(json!({ "id": uid.to_string(), "email": email, "name": name, "verified": verified }))
}

// ponytail: sessions never expire; add an expires_at column and a cleanup if that matters
async fn session(c: &mut PgConnection, uid: i64) -> Result<Value, ApiError> {
    let user = user_json(c, uid).await?;
    if user["verified"] == true {
        apply_invites(c, uid, user["email"].as_str().unwrap()).await?;
    }
    let token = B64.encode(rand::random::<[u8; 32]>());
    sqlx::query("INSERT INTO sessions(token_hash,user_id,created_at) VALUES($1,$2,$3)")
        .bind(hash_token(&token))
        .bind(uid)
        .bind(now_ms())
        .execute(c)
        .await?;
    Ok(json!({ "token": token, "user": user }))
}

const CODE_TTL_MS: i64 = 30 * 60 * 1000;
const MAX_ATTEMPTS: i32 = 5;

async fn issue_code(c: &mut PgConnection, uid: i64) -> sqlx::Result<String> {
    let code = format!("{:06}", rand::random_range(0..1_000_000));
    sqlx::query(
        "INSERT INTO email_codes(user_id,code_hash,expires_at,attempts) VALUES($1,$2,$3,0) \
         ON CONFLICT (user_id) DO UPDATE SET code_hash=EXCLUDED.code_hash, expires_at=EXCLUDED.expires_at, attempts=0",
    )
    .bind(uid)
    .bind(hash_token(&code))
    .bind(now_ms() + CODE_TTL_MS)
    .execute(c)
    .await?;
    Ok(code)
}

pub struct Mail {
    transport: lettre::AsyncSmtpTransport<lettre::Tokio1Executor>,
    from: lettre::message::Mailbox,
}

impl Mail {
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("FLOKI_SMTP_URL").ok().filter(|u| !u.is_empty())?;
        let from = std::env::var("FLOKI_MAIL_FROM").expect("FLOKI_MAIL_FROM is required with FLOKI_SMTP_URL");
        Some(Mail {
            transport: lettre::AsyncSmtpTransport::<lettre::Tokio1Executor>::from_url(&url).expect("invalid FLOKI_SMTP_URL").build(),
            from: from.parse().expect("invalid FLOKI_MAIL_FROM"),
        })
    }
}

async fn send_code(st: &AppState, email: &str, code: &str) {
    let Some(mail) = &st.mail else {
        // ponytail: dev fallback, no SMTP configured; the code only goes to the server log
        println!("Verification code for {email}: {code}");
        return;
    };
    use lettre::AsyncTransport;
    let msg = email.parse().ok().and_then(|to| {
        lettre::Message::builder()
            .from(mail.from.clone())
            .to(to)
            .subject("Your Floki verification code")
            .body(format!("Your verification code is {code}. It expires in 30 minutes."))
            .ok()
    });
    match msg {
        Some(m) => {
            if let Err(e) = mail.transport.send(m).await {
                eprintln!("failed to send verification email to {email}: {e}");
            }
        }
        None => eprintln!("could not build verification email for {email}"),
    }
}

#[derive(Deserialize)]
pub struct Creds {
    email: String,
    password: String,
    #[serde(default)]
    name: String,
}

pub async fn signup(State(st): State<AppState>, Json(b): Json<Creds>) -> Res {
    let email = norm_email(&b.email).ok_or_else(|| err(400, "Invalid email address"))?;
    if b.password.chars().count() < 8 {
        return Err(err(400, "Password must be at least 8 characters"));
    }
    let hash = hash_pw(b.password).await?;
    let name = if b.name.trim().is_empty() { email.clone() } else { b.name.trim().to_string() };
    let mut tx = st.db.begin().await?;
    let uid = match sqlx::query_scalar::<_, i64>(
        "INSERT INTO users(email,name,password_hash,created_at) VALUES($1,$2,$3,$4) RETURNING id",
    )
    .bind(&email)
    .bind(&name)
    .bind(&hash)
    .bind(now_ms())
    .fetch_one(&mut *tx)
    .await
    {
        Err(e) if is_unique(&e) => return Err(err(409, "An account with this email already exists")),
        r => r?,
    };
    let sess = session(&mut tx, uid).await?;
    let code = issue_code(&mut tx, uid).await?;
    tx.commit().await?;
    send_code(&st, &email, &code).await;
    Ok(Json(sess))
}

// ponytail: no login rate limiting or constant-time unknown-email path; add a per-IP limiter before exposing publicly
pub async fn login(State(st): State<AppState>, Json(b): Json<Creds>) -> Res {
    let bad = || err(401, "Invalid email or password");
    let email = norm_email(&b.email).ok_or_else(bad)?;
    let (uid, hash): (i64, Option<String>) = sqlx::query_as("SELECT id, password_hash FROM users WHERE email=$1")
        .bind(&email)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(bad)?;
    if !verify_pw(b.password, hash.ok_or_else(bad)?).await {
        return Err(bad());
    }
    let mut tx = st.db.begin().await?;
    let sess = session(&mut tx, uid).await?;
    tx.commit().await?;
    Ok(Json(sess))
}

pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    let token = bearer(&headers)?;
    let n = sqlx::query("DELETE FROM sessions WHERE token_hash=$1").bind(hash_token(token)).execute(&st.db).await?;
    if n.rows_affected() == 0 {
        return Err(err(401, "Invalid or expired session"));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(State(st): State<AppState>, u: AuthUser) -> Res {
    let mut c = st.db.acquire().await?;
    let user = user_json(&mut c, u.id).await?;
    let teams: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT t.id,t.name,m.role FROM team_members m JOIN teams t ON t.id=m.team_id WHERE m.user_id=$1 ORDER BY lower(t.name), t.id",
    )
    .bind(u.id)
    .fetch_all(&mut *c)
    .await?;
    let teams: Vec<Value> =
        teams.into_iter().map(|(id, name, role)| json!({ "id": id.to_string(), "name": name, "role": role })).collect();
    Ok(Json(json!({ "user": user, "teams": teams })))
}

#[derive(Deserialize)]
pub struct CodeReq {
    code: String,
}

pub async fn verify(State(st): State<AppState>, u: AuthUser, Json(b): Json<CodeReq>) -> Res {
    let mut tx = st.db.begin().await?;
    let row: Option<(String, i64, i32)> =
        sqlx::query_as("SELECT code_hash, expires_at, attempts FROM email_codes WHERE user_id=$1 FOR UPDATE")
            .bind(u.id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((hash, expires, attempts)) = row else {
        let user = user_json(&mut tx, u.id).await?;
        if user["verified"] == true {
            return Ok(Json(json!({ "user": user })));
        }
        return Err(err(400, "No code pending, request a new code"));
    };
    if attempts >= MAX_ATTEMPTS {
        return Err(err(400, "Too many attempts, request a new code"));
    }
    if expires < now_ms() {
        return Err(err(400, "Code expired, request a new code"));
    }
    if hash != hash_token(b.code.trim()) {
        sqlx::query("UPDATE email_codes SET attempts=attempts+1 WHERE user_id=$1").bind(u.id).execute(&mut *tx).await?;
        // The attempt counter must persist even though the request fails.
        tx.commit().await?;
        return Err(err(400, "Incorrect code"));
    }
    sqlx::query("UPDATE users SET email_verified=TRUE WHERE id=$1").bind(u.id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM email_codes WHERE user_id=$1").bind(u.id).execute(&mut *tx).await?;
    let user = user_json(&mut tx, u.id).await?;
    apply_invites(&mut tx, u.id, user["email"].as_str().unwrap()).await?;
    tx.commit().await?;
    Ok(Json(json!({ "user": user })))
}

// ponytail: no resend cooldown, so a client can spam mail; add a per-user minimum interval
pub async fn verify_resend(State(st): State<AppState>, u: AuthUser) -> Result<StatusCode, ApiError> {
    let mut c = st.db.acquire().await?;
    let user = user_json(&mut c, u.id).await?;
    if user["verified"] == true {
        return Err(err(400, "Email is already verified"));
    }
    let code = issue_code(&mut c, u.id).await?;
    drop(c);
    send_code(&st, user["email"].as_str().unwrap(), &code).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn google_config(State(st): State<AppState>) -> Res {
    match &st.google {
        Some((id, _)) => Ok(Json(json!({ "client_id": id }))),
        None => Err(err(404, "Google sign-in is not configured on this server")),
    }
}

#[derive(Deserialize)]
pub struct GoogleReq {
    code: String,
    code_verifier: String,
    redirect_uri: String,
}

pub async fn google_login(State(st): State<AppState>, Json(b): Json<GoogleReq>) -> Res {
    let (cid, secret) = st.google.clone().ok_or_else(|| err(404, "Google sign-in is not configured on this server"))?;
    let net = |_| err(401, "Could not reach Google to verify sign-in");
    let resp = st
        .http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", &cid),
            ("client_secret", &secret),
            ("code", &b.code),
            ("code_verifier", &b.code_verifier),
            ("redirect_uri", &b.redirect_uri),
        ])
        .send()
        .await
        .map_err(net)?;
    let tok: Value = resp.json().await.map_err(net)?;
    let id_token = tok["id_token"].as_str().ok_or_else(|| {
        let why = tok["error_description"].as_str().or(tok["error"].as_str()).unwrap_or("no id_token returned");
        err(401, &format!("Google rejected the sign-in: {why}"))
    })?;

    // ponytail: tokeninfo instead of local JWKS verification; adds a round trip per login
    let resp = st
        .http
        .get("https://oauth2.googleapis.com/tokeninfo")
        .query(&[("id_token", id_token)])
        .send()
        .await
        .map_err(net)?;
    if !resp.status().is_success() {
        return Err(err(401, "Google could not verify the ID token"));
    }
    let info: Value = resp.json().await.map_err(net)?;
    let s = |k: &str| info[k].as_str().unwrap_or("").to_string();
    let exp = info["exp"].as_str().and_then(|v| v.parse::<i64>().ok()).or(info["exp"].as_i64()).unwrap_or(0);
    let verified = info["email_verified"].as_str() == Some("true") || info["email_verified"].as_bool() == Some(true);
    if s("aud") != cid
        || !matches!(s("iss").as_str(), "accounts.google.com" | "https://accounts.google.com")
        || exp * 1000 <= now_ms()
    {
        return Err(err(401, "Google ID token is invalid or expired"));
    }
    if !verified {
        return Err(err(401, "Your Google email address is not verified"));
    }
    let sub = s("sub");
    let email = norm_email(&s("email")).ok_or_else(|| err(401, "Google account has no usable email"))?;
    if sub.is_empty() {
        return Err(err(401, "Google ID token has no subject"));
    }

    let mut tx = st.db.begin().await?;
    let by_sub: Option<i64> =
        sqlx::query_scalar("SELECT id FROM users WHERE google_sub=$1").bind(&sub).fetch_optional(&mut *tx).await?;
    let uid = match by_sub {
        Some(id) => id,
        None => {
            let by_email: Option<(i64, bool)> =
                sqlx::query_as("SELECT id, email_verified FROM users WHERE email=$1 FOR UPDATE")
                    .bind(&email)
                    .fetch_optional(&mut *tx)
                    .await?;
            match by_email {
                Some((id, verified)) => {
                    if !verified {
                        // The unverified registrant may be an attacker squatting the address; Google proves ownership.
                        sqlx::query("UPDATE users SET password_hash=NULL WHERE id=$1").bind(id).execute(&mut *tx).await?;
                        sqlx::query("DELETE FROM sessions WHERE user_id=$1").bind(id).execute(&mut *tx).await?;
                    }
                    sqlx::query("UPDATE users SET google_sub=$1 WHERE id=$2").bind(&sub).bind(id).execute(&mut *tx).await?;
                    id
                }
                None => {
                    let name = Some(s("name")).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| email.clone());
                    sqlx::query_scalar(
                        "INSERT INTO users(email,name,google_sub,email_verified,created_at) VALUES($1,$2,$3,TRUE,$4) RETURNING id",
                    )
                    .bind(&email)
                    .bind(&name)
                    .bind(&sub)
                    .bind(now_ms())
                    .fetch_one(&mut *tx)
                    .await?
                }
            }
        }
    };
    sqlx::query("UPDATE users SET email_verified=TRUE WHERE id=$1").bind(uid).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM email_codes WHERE user_id=$1").bind(uid).execute(&mut *tx).await?;
    let sess = session(&mut tx, uid).await?;
    tx.commit().await?;
    Ok(Json(sess))
}
