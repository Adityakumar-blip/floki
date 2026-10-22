mod auth;
mod routes;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use sqlx::{postgres::PgPoolOptions, PgPool};
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// Connects using `FLOKI_DB_MAX_CONNECTIONS` (default 10) and applies pending migrations.
pub async fn connect(url: &str) -> Result<PgPool, Box<dyn std::error::Error>> {
    let max = std::env::var("FLOKI_DB_MAX_CONNECTIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
    let pool = PgPoolOptions::new().max_connections(max).connect(url).await?;
    migrate(&pool).await?;
    Ok(pool)
}

#[derive(Clone)]
pub struct AppState {
    db: PgPool,
    google: Option<(String, String)>,
    http: reqwest::Client,
    mail: Option<Arc<auth::Mail>>,
}

pub struct ApiError(StatusCode, String);

fn err(code: u16, msg: &str) -> ApiError {
    ApiError(StatusCode::from_u16(code).unwrap(), msg.into())
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        eprintln!("db error: {e}");
        err(500, "Internal server error")
    }
}

type Res<T = Json<Value>> = Result<T, ApiError>;

pub fn app(db: PgPool, google: Option<(String, String)>) -> Router {
    use auth::*;
    use routes::*;
    let st = AppState { db, google, http: reqwest::Client::new(), mail: auth::Mail::from_env().map(Arc::new) };
    Router::new()
        .route("/api/auth/signup", post(signup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/google/config", get(google_config))
        .route("/api/auth/google", post(google_login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/verify", post(verify))
        .route("/api/auth/verify/resend", post(verify_resend))
        .route("/api/me", get(me))
        .route("/api/teams", post(create_team))
        .route("/api/teams/{id}/members", get(team_members).post(team_add))
        .route("/api/teams/{id}/members/{uid}", patch(team_patch).delete(team_remove))
        .route("/api/teams/{id}/invites/{email}", delete(team_invite_delete))
        .route("/api/workspaces", get(ws_list).post(ws_create))
        .route("/api/workspaces/{id}", get(ws_get).patch(ws_rename).delete(ws_delete))
        .route("/api/workspaces/{id}/version", get(ws_version))
        .route("/api/workspaces/{id}/data", put(ws_put))
        .route("/api/workspaces/{id}/members", get(ws_members).post(ws_add))
        .route("/api/workspaces/{id}/members/{uid}", patch(ws_patch).delete(ws_remove))
        .route("/api/workspaces/{id}/invites/{email}", delete(ws_invite_delete))
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024))
        .with_state(st)
}
