use std::env;

#[tokio::main]
async fn main() {
    let bind = env::var("FLOKI_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let url = env::var("DATABASE_URL").expect("DATABASE_URL is required, e.g. postgres://user:pass@localhost/floki");
    let google = match (env::var("GOOGLE_CLIENT_ID"), env::var("GOOGLE_CLIENT_SECRET")) {
        (Ok(id), Ok(secret)) if !id.is_empty() && !secret.is_empty() => Some((id, secret)),
        _ => None,
    };
    let db = floki_server::connect(&url).await.expect("connect to database and run migrations");
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!(
        "floki-server listening on http://{} (google sign-in {})",
        listener.local_addr().unwrap(),
        if google.is_some() { "enabled" } else { "disabled" }
    );
    axum::serve(listener, floki_server::app(db, google)).await.unwrap();
}
