use axum::{body::Body, http::Request, Router};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Ctx {
    app: Router,
    pool: sqlx::PgPool,
    admin: sqlx::PgPool,
    schema: String,
}

impl std::ops::Deref for Ctx {
    type Target = Router;
    fn deref(&self) -> &Router {
        &self.app
    }
}

impl Ctx {
    async fn done(self) {
        self.pool.close().await;
        let _ = sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema)).execute(&self.admin).await;
    }
}

// Each test gets its own schema so tests run in parallel; skipped (passing) without TEST_DATABASE_URL.
async fn app() -> Option<Ctx> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return None;
    };
    let schema = format!("t_{:x}", rand::random::<u64>());
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query(&format!("CREATE SCHEMA {schema}")).execute(&admin).await.unwrap();
    let s = schema.clone();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |conn, _| {
            let q = format!("SET search_path TO {s}");
            Box::pin(async move {
                sqlx::query(&q).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    floki_server::migrate(&pool).await.unwrap();
    Some(Ctx { app: floki_server::app(pool.clone(), None), pool, admin, schema })
}

async fn plant_code(ctx: &Ctx, uid: &str, code: &str, expires_in_ms: i64) {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use sha2::{Digest, Sha256};
    let hash = B64.encode(Sha256::digest(code.as_bytes()));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    sqlx::query(
        "INSERT INTO email_codes(user_id,code_hash,expires_at,attempts) VALUES($1,$2,$3,0) \
         ON CONFLICT (user_id) DO UPDATE SET code_hash=EXCLUDED.code_hash, expires_at=EXCLUDED.expires_at, attempts=0",
    )
    .bind(uid.parse::<i64>().unwrap())
    .bind(hash)
    .bind(now + expires_in_ms)
    .execute(&ctx.pool)
    .await
    .unwrap();
}

async fn verify(ctx: &Ctx, tok: &str, uid: &str) {
    plant_code(ctx, uid, "123456", 60_000).await;
    let (s, v) = call(ctx, "POST", "/api/auth/verify", Some(tok), json!({"code": "123456"})).await;
    assert_eq!((s, v["user"]["verified"].as_bool()), (200, Some(true)), "{v}");
}

async fn call(app: &Router, method: &str, uri: &str, tok: Option<&str>, body: Value) -> (u16, Value) {
    let mut req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    if let Some(t) = tok {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let res = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn signup(app: &Ctx, email: &str) -> (String, String) {
    let (tok, id) = signup_unverified(app, email).await;
    verify(app, &tok, &id).await;
    (tok, id)
}

async fn signup_unverified(app: &Router, email: &str) -> (String, String) {
    let (s, v) = call(app, "POST", "/api/auth/signup", None, json!({"email": email, "password": "password1", "name": "N"})).await;
    assert_eq!(s, 200, "{v}");
    (v["token"].as_str().unwrap().into(), v["user"]["id"].as_str().unwrap().into())
}

#[tokio::test]
async fn auth_flow() {
    let Some(app) = app().await else { return };
    let (tok, id) = signup_unverified(&app, "  A@Example.com ").await;
    let (_, v) = call(&app, "GET", "/api/me", Some(&tok), Value::Null).await;
    assert_eq!(v["user"]["verified"], false);
    let (s, v) = call(&app, "POST", "/api/auth/signup", None, json!({"email": "a@example.com", "password": "password1", "name": "x"})).await;
    assert_eq!((s, v["error"].is_string()), (409, true));
    let (s, _) = call(&app, "POST", "/api/auth/signup", None, json!({"email": "bad", "password": "password1", "name": "x"})).await;
    assert_eq!(s, 400);
    let (s, _) = call(&app, "POST", "/api/auth/signup", None, json!({"email": "b@example.com", "password": "short", "name": "x"})).await;
    assert_eq!(s, 400);

    let (s, v) = call(&app, "POST", "/api/auth/login", None, json!({"email": "a@example.com", "password": "password1"})).await;
    assert_eq!(s, 200);
    assert_eq!(v["user"]["id"], id);
    let (s, v1) = call(&app, "POST", "/api/auth/login", None, json!({"email": "a@example.com", "password": "wrongwrong"})).await;
    let (s2, v2) = call(&app, "POST", "/api/auth/login", None, json!({"email": "nobody@example.com", "password": "wrongwrong"})).await;
    assert_eq!((s, s2), (401, 401));
    assert_eq!(v1, v2);

    let (s, v) = call(&app, "GET", "/api/me", Some(&tok), Value::Null).await;
    assert_eq!((s, v["user"]["email"].as_str()), (200, Some("a@example.com")));
    assert_eq!(call(&app, "GET", "/api/me", None, Value::Null).await.0, 401);
    assert_eq!(call(&app, "GET", "/api/me", Some("nope"), Value::Null).await.0, 401);

    assert_eq!(call(&app, "POST", "/api/auth/logout", Some(&tok), Value::Null).await.0, 204);
    assert_eq!(call(&app, "GET", "/api/me", Some(&tok), Value::Null).await.0, 401);
    assert_eq!(call(&app, "GET", "/api/auth/google/config", None, Value::Null).await.0, 404);
    app.done().await;
}

#[tokio::test]
async fn teams_and_invites() {
    let Some(app) = app().await else { return };
    let (a, a_id) = signup(&app, "a@x.com").await;
    let (b, b_id) = signup(&app, "b@x.com").await;
    let (s, t) = call(&app, "POST", "/api/teams", Some(&a), json!({"name": "T"})).await;
    assert_eq!((s, t["role"].as_str()), (200, Some("admin")));
    let tid = t["id"].as_str().unwrap();

    assert_eq!(call(&app, "GET", &format!("/api/teams/{tid}/members"), Some(&b), Value::Null).await.0, 404);
    let (_, v) = call(&app, "POST", &format!("/api/teams/{tid}/members"), Some(&a), json!({"email": "b@x.com", "role": "member"})).await;
    assert_eq!(v["status"], "added");
    let (_, v) = call(&app, "POST", &format!("/api/teams/{tid}/members"), Some(&a), json!({"email": "c@x.com", "role": "member"})).await;
    assert_eq!(v["status"], "invited");
    assert_eq!(call(&app, "POST", &format!("/api/teams/{tid}/members"), Some(&b), json!({"email": "d@x.com", "role": "member"})).await.0, 403);

    let (c, _) = signup(&app, "C@x.com").await;
    let (_, me) = call(&app, "GET", "/api/me", Some(&c), Value::Null).await;
    assert_eq!(me["teams"][0]["id"], tid);
    let (_, v) = call(&app, "GET", &format!("/api/teams/{tid}/members"), Some(&a), Value::Null).await;
    assert_eq!((v["members"].as_array().unwrap().len(), v["invites"].as_array().unwrap().len()), (3, 0));

    let last = |uid: &str| format!("/api/teams/{tid}/members/{uid}");
    assert_eq!(call(&app, "PATCH", &last(&a_id), Some(&a), json!({"role": "member"})).await.0, 400);
    assert_eq!(call(&app, "DELETE", &last(&a_id), Some(&a), Value::Null).await.0, 400);
    assert_eq!(call(&app, "DELETE", &last(&a_id), Some(&b), Value::Null).await.0, 403);
    assert_eq!(call(&app, "DELETE", &last(&b_id), Some(&b), Value::Null).await.0, 204);
    app.done().await;
}

#[tokio::test]
async fn workspaces() {
    let Some(app) = app().await else { return };
    let (a, _) = signup(&app, "a@x.com").await;
    let (b, b_id) = signup(&app, "b@x.com").await;
    let (c, _) = signup(&app, "c@x.com").await;

    let (s, w) = call(&app, "POST", "/api/workspaces", Some(&a), json!({"name": "W", "team_id": null, "data": {"k": 1}})).await;
    assert_eq!((s, w["role"].as_str(), w["version"].as_i64()), (200, Some("admin"), Some(1)));
    let wid = w["id"].as_str().unwrap().to_string();
    let url = format!("/api/workspaces/{wid}");

    let (_, l) = call(&app, "GET", "/api/workspaces", Some(&a), Value::Null).await;
    assert_eq!(l.as_array().unwrap().len(), 1);
    let (_, g) = call(&app, "GET", &url, Some(&a), Value::Null).await;
    assert_eq!(g["data"], json!({"k": 1}));

    let put = |tok: &str, v: i64| {
        let (app, url, tok) = (app.clone(), format!("{url}/data"), tok.to_string());
        async move { call(&app, "PUT", &url, Some(&tok), json!({"base_version": v, "data": {"k": v + 1}})).await }
    };
    assert_eq!(put(&a, 1).await, (200, json!({"version": 2})));
    let (s, v) = put(&a, 1).await;
    assert_eq!((s, v["version"].as_i64()), (409, Some(2)));
    assert_eq!(call(&app, "GET", &format!("{url}/version"), Some(&a), Value::Null).await.1, json!({"version": 2}));

    assert_eq!(call(&app, "GET", &url, Some(&b), Value::Null).await.0, 404);
    assert_eq!(put(&b, 2).await.0, 404);

    let (_, v) = call(&app, "POST", &format!("{url}/members"), Some(&a), json!({"email": "b@x.com", "role": "viewer"})).await;
    assert_eq!(v["status"], "added");
    assert_eq!(call(&app, "GET", &url, Some(&b), Value::Null).await.0, 200);
    assert_eq!(put(&b, 2).await.0, 403);
    assert_eq!(call(&app, "PATCH", &url, Some(&b), json!({"name": "Z"})).await.0, 403);
    assert_eq!(call(&app, "PATCH", &format!("{url}/members/{b_id}"), Some(&a), json!({"role": "editor"})).await.0, 204);
    assert_eq!(put(&b, 2).await.0, 200);

    let admin = format!("{url}/members/{}", call(&app, "GET", "/api/me", Some(&a), Value::Null).await.1["user"]["id"].as_str().unwrap());
    assert_eq!(call(&app, "DELETE", &admin, Some(&a), Value::Null).await.0, 400);

    // team inheritance
    let (_, t) = call(&app, "POST", "/api/teams", Some(&a), json!({"name": "T"})).await;
    let tid = t["id"].as_str().unwrap();
    assert_eq!(call(&app, "POST", "/api/workspaces", Some(&c), json!({"name": "X", "team_id": tid, "data": {}})).await.0, 403);
    let (_, tw) = call(&app, "POST", "/api/workspaces", Some(&a), json!({"name": "TW", "team_id": tid, "data": {}})).await;
    let tw_url = format!("/api/workspaces/{}", tw["id"].as_str().unwrap());
    assert_eq!(call(&app, "GET", &tw_url, Some(&c), Value::Null).await.0, 404);
    call(&app, "POST", &format!("/api/teams/{tid}/members"), Some(&a), json!({"email": "c@x.com", "role": "member"})).await;
    let (s, g) = call(&app, "GET", &tw_url, Some(&c), Value::Null).await;
    assert_eq!((s, g["role"].as_str(), g["team_name"].as_str()), (200, Some("editor"), Some("T")));
    assert_eq!(call(&app, "PUT", &format!("{tw_url}/data"), Some(&c), json!({"base_version": 1, "data": {}})).await.0, 200);
    assert_eq!(call(&app, "DELETE", &tw_url, Some(&c), Value::Null).await.0, 403);

    assert_eq!(call(&app, "DELETE", &url, Some(&a), Value::Null).await.0, 204);
    assert_eq!(call(&app, "GET", &url, Some(&a), Value::Null).await.0, 404);
    app.done().await;
}

#[tokio::test]
async fn workspace_invite_applied_on_login() {
    let Some(app) = app().await else { return };
    let (a, _) = signup(&app, "a@x.com").await;
    let (_, w) = call(&app, "POST", "/api/workspaces", Some(&a), json!({"name": "W", "team_id": null, "data": {}})).await;
    let url = format!("/api/workspaces/{}", w["id"].as_str().unwrap());
    let (_, v) = call(&app, "POST", &format!("{url}/members"), Some(&a), json!({"email": "new@x.com", "role": "viewer"})).await;
    assert_eq!(v["status"], "invited");
    let (b, _) = signup(&app, "new@x.com").await;
    let (_, g) = call(&app, "GET", &url, Some(&b), Value::Null).await;
    assert_eq!(g["role"], "viewer");
    app.done().await;
}

#[tokio::test]
async fn unverified_gets_no_invites_until_verified() {
    let Some(app) = app().await else { return };
    let (a, _) = signup(&app, "a@x.com").await;
    let (_, t) = call(&app, "POST", "/api/teams", Some(&a), json!({"name": "T"})).await;
    let tid = t["id"].as_str().unwrap();
    call(&app, "POST", &format!("/api/teams/{tid}/members"), Some(&a), json!({"email": "v@x.com", "role": "member"})).await;

    let (v, vid) = signup_unverified(&app, "v@x.com").await;
    let (_, me) = call(&app, "GET", "/api/me", Some(&v), Value::Null).await;
    assert_eq!(me["teams"].as_array().unwrap().len(), 0);
    // A fresh login while still unverified must not apply invites either.
    let (_, l) = call(&app, "POST", "/api/auth/login", None, json!({"email": "v@x.com", "password": "password1"})).await;
    let (_, me) = call(&app, "GET", "/api/me", Some(l["token"].as_str().unwrap()), Value::Null).await;
    assert_eq!(me["teams"].as_array().unwrap().len(), 0);

    verify(&app, &v, &vid).await;
    let (_, me) = call(&app, "GET", "/api/me", Some(&v), Value::Null).await;
    assert_eq!(me["teams"][0]["id"], tid);
    app.done().await;
}

#[tokio::test]
async fn code_errors_and_attempt_limit() {
    let Some(app) = app().await else { return };
    let (t, id) = signup_unverified(&app, "u@x.com").await;
    let bad = |c: &'static str| call(&app, "POST", "/api/auth/verify", Some(&t), json!({"code": c}));
    plant_code(&app, &id, "123456", -1000).await;
    let (s, v) = bad("123456").await;
    assert_eq!((s, v["error"].as_str()), (400, Some("Code expired, request a new code")));

    plant_code(&app, &id, "123456", 60_000).await;
    for _ in 0..5 {
        let (s, v) = bad("000000").await;
        assert_eq!((s, v["error"].as_str()), (400, Some("Incorrect code")));
    }
    let (_, v) = bad("123456").await;
    assert_eq!(v["error"], "Too many attempts, request a new code");

    assert_eq!(call(&app, "POST", "/api/auth/verify/resend", Some(&t), Value::Null).await.0, 204);
    verify(&app, &t, &id).await;
    assert_eq!(call(&app, "POST", "/api/auth/verify/resend", Some(&t), Value::Null).await.0, 400);
    app.done().await;
}

#[tokio::test]
async fn adding_unverified_user_creates_invite() {
    let Some(app) = app().await else { return };
    let (a, _) = signup(&app, "a@x.com").await;
    let (u, uid) = signup_unverified(&app, "u@x.com").await;
    let (_, w) = call(&app, "POST", "/api/workspaces", Some(&a), json!({"name": "W", "team_id": null, "data": {}})).await;
    let url = format!("/api/workspaces/{}", w["id"].as_str().unwrap());
    let (_, v) = call(&app, "POST", &format!("{url}/members"), Some(&a), json!({"email": "u@x.com", "role": "editor"})).await;
    assert_eq!(v["status"], "invited");
    assert_eq!(call(&app, "GET", &url, Some(&u), Value::Null).await.0, 404);
    verify(&app, &u, &uid).await;
    assert_eq!(call(&app, "GET", &url, Some(&u), Value::Null).await.0, 200);
    app.done().await;
}

#[tokio::test]
async fn concurrent_puts_one_wins() {
    let Some(app) = app().await else { return };
    let (a, _) = signup(&app, "a@x.com").await;
    let (_, w) = call(&app, "POST", "/api/workspaces", Some(&a), json!({"name": "W", "team_id": null, "data": {}})).await;
    let uri = format!("/api/workspaces/{}/data", w["id"].as_str().unwrap());
    let body = json!({"base_version": 1, "data": {"x": 1}});
    let (r1, r2) = tokio::join!(
        call(&app, "PUT", &uri, Some(&a), body.clone()),
        call(&app, "PUT", &uri, Some(&a), body.clone())
    );
    let mut codes = [r1.0, r2.0];
    codes.sort();
    assert_eq!(codes, [200, 409]);
    app.done().await;
}

#[tokio::test]
async fn concurrent_demotions_keep_one_admin() {
    let Some(app) = app().await else { return };
    let (a, a_id) = signup(&app, "a@x.com").await;
    let (b, b_id) = signup(&app, "b@x.com").await;
    let (_, t) = call(&app, "POST", "/api/teams", Some(&a), json!({"name": "T"})).await;
    let tid = t["id"].as_str().unwrap();
    call(&app, "POST", &format!("/api/teams/{tid}/members"), Some(&a), json!({"email": "b@x.com", "role": "admin"})).await;
    let demote = |tok: String, uid: String| {
        let (app, uri) = (app.app.clone(), format!("/api/teams/{tid}/members/{uid}"));
        async move { call(&app, "PATCH", &uri, Some(&tok), json!({"role": "member"})).await.0 }
    };
    let (x, y) = tokio::join!(demote(a.clone(), b_id.clone()), demote(b.clone(), a_id.clone()));
    let mut codes = [x, y];
    codes.sort();
    // The loser is rejected either by the last-admin guard (400) or because it was already demoted (403).
    assert_eq!(codes[0], 204);
    assert!(codes[1] == 400 || codes[1] == 403, "{codes:?}");
    let admins: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM team_members WHERE role='admin'").fetch_one(&app.pool).await.unwrap();
    assert_eq!(admins, 1);
    app.done().await;
}
