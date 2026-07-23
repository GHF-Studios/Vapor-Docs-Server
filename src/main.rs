use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "127.0.0.1:7112";
const DEFAULT_STATE_DIR: &str = "state/docs";
const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    state_dir: Arc<PathBuf>,
    admin_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = env::var("VAPOR_DOCS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let state_dir =
        PathBuf::from(env::var("VAPOR_DOCS_STATE").unwrap_or_else(|_| DEFAULT_STATE_DIR.into()));
    fs::create_dir_all(state_dir.join("current")).await?;

    let state = AppState {
        state_dir: Arc::new(state_dir),
        admin_token: env::var("VAPOR_DOCS_ADMIN_TOKEN")
            .ok()
            .filter(|value| !value.is_empty()),
    };
    let app = Router::new()
        .route("/", get(serve_current_docs))
        .route("/index.html", get(serve_current_docs))
        .route("/healthz", get(healthz))
        .route("/v1/current", post(upload_current_docs))
        .route("/v1/export", get(export_docs))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state);

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("vapor-docs-server listening on {bind}");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn serve_current_docs(State(state): State<AppState>) -> impl IntoResponse {
    let index = state.state_dir.join("current").join("index.html");
    let body = fs::read_to_string(index).await.unwrap_or_else(|_| {
        "<!doctype html><title>Vapor Docs</title><h1>Vapor Docs</h1><p>No docs have been published yet.</p>\n"
            .to_string()
    });
    Html(body)
}

async fn upload_current_docs(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        );
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty docs upload\n".to_string());
    }

    let current = state.state_dir.join("current");
    if let Err(error) = fs::create_dir_all(&current).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: failed to create state directory: {error}\n"),
        );
    }
    if let Err(error) = fs::write(current.join("index.html"), &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: failed to write current docs: {error}\n"),
        );
    }

    let metadata = format!(
        "published_at_unix = {}\nbytes = {}\nformat = \"single-html\"\n",
        unix_now(),
        body.len()
    );
    if let Err(error) = fs::write(current.join("metadata.toml"), metadata).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: failed to write metadata: {error}\n"),
        );
    }

    (
        StatusCode::CREATED,
        "docs: current docs updated\n".to_string(),
    )
}

async fn export_docs(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        );
    }

    let metadata = state.state_dir.join("current").join("metadata.toml");
    let index = state.state_dir.join("current").join("index.html");
    let metadata = fs::read_to_string(metadata).await.unwrap_or_default();
    let index = fs::read_to_string(index).await.unwrap_or_default();
    let body = format!(
        "# Vapor docs export scaffold\n\n[metadata]\n{metadata}\n--- index.html ---\n{index}"
    );

    (StatusCode::OK, body)
}

fn authorized(headers: &HeaderMap, expected: &Option<String>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {expected}"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
