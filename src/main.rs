use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use flate2::read::GzDecoder;
use std::env;
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tar::Archive;
use tokio::fs;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

const DEFAULT_BIND: &str = "127.0.0.1:7112";
const DEFAULT_STATE_DIR: &str = "state/docs";
const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

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
    let current_dir = state_dir.join("current");
    fs::create_dir_all(&current_dir).await?;

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
        .route("/v1/current.tar.gz", post(upload_current_docs_archive))
        .route("/v1/export", get(export_docs))
        .fallback_service(ServeDir::new(current_dir).append_index_html_on_directories(true))
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

    if let Err(error) = install_single_file_docs(&state.state_dir, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: failed to write current docs: {error}\n"),
        );
    }

    (
        StatusCode::CREATED,
        "docs: current docs updated\n".to_string(),
    )
}

async fn upload_current_docs_archive(
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

    let state_dir = (*state.state_dir).clone();
    let result = tokio::task::spawn_blocking(move || install_archive_docs(&state_dir, &body)).await;
    let report = match result {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("docs: failed to install archive: {error}\n"),
            );
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("docs: archive installer task failed: {error}\n"),
            );
        }
    };

    (
        StatusCode::CREATED,
        format!(
            "docs: current docs tree updated\nfiles = {}\nbytes = {}\n",
            report.files, report.bytes
        ),
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

async fn install_single_file_docs(state_dir: &Path, body: &[u8]) -> Result<(), String> {
    let current = state_dir.join("current");
    fs::create_dir_all(&current)
        .await
        .map_err(|e| e.to_string())?;
    fs::write(current.join("index.html"), body)
        .await
        .map_err(|e| e.to_string())?;

    let metadata = format!(
        "published_at_unix = {}\nbytes = {}\nformat = \"single-html\"\n",
        unix_now(),
        body.len()
    );
    fs::write(current.join("metadata.toml"), metadata)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

struct ArchiveInstallReport {
    files: u64,
    bytes: u64,
}

fn install_archive_docs(state_dir: &Path, body: &[u8]) -> Result<ArchiveInstallReport, String> {
    std::fs::create_dir_all(state_dir).map_err(|error| error.to_string())?;
    let staging = tempfile::Builder::new()
        .prefix(".current-upload-")
        .tempdir_in(state_dir)
        .map_err(|error| {
            format!(
                "failed to create staging directory in '{}': {error}",
                state_dir.display()
            )
        })?;
    let staging_current = staging.path().join("current");
    std::fs::create_dir_all(&staging_current).map_err(|error| error.to_string())?;

    let mut archive = Archive::new(GzDecoder::new(Cursor::new(body)));
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let entries = archive
        .entries()
        .map_err(|error| format!("failed to read archive entries: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("failed to read archive entry: {error}"))?;
        let entry_path = entry
            .path()
            .map_err(|error| format!("failed to read archive path: {error}"))?;
        let relative = safe_archive_path(&entry_path)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = staging_current.join(relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            std::fs::create_dir_all(&destination).map_err(|error| {
                format!(
                    "failed to create directory '{}': {error}",
                    destination.display()
                )
            })?;
        } else if entry_type.is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("failed to create directory '{}': {error}", parent.display())
                })?;
            }
            let mut output = std::fs::File::create(&destination).map_err(|error| {
                format!("failed to create file '{}': {error}", destination.display())
            })?;
            let copied = std::io::copy(&mut entry, &mut output).map_err(|error| {
                format!("failed to write file '{}': {error}", destination.display())
            })?;
            output.flush().map_err(|error| {
                format!("failed to flush file '{}': {error}", destination.display())
            })?;
            files += 1;
            bytes += copied;
        } else {
            return Err(format!(
                "unsupported archive entry type for '{}'",
                entry_path.display()
            ));
        }
    }

    if !staging_current.join("index.html").is_file() {
        return Err("docs archive must contain index.html at its root".to_string());
    }

    let metadata = format!(
        "published_at_unix = {}\nbytes = {}\nfiles = {}\nformat = \"tar-gzip-tree\"\n",
        unix_now(),
        bytes,
        files
    );
    std::fs::write(staging_current.join("metadata.toml"), metadata)
        .map_err(|error| format!("failed to write docs metadata: {error}"))?;

    replace_current_tree(state_dir, &staging_current)?;

    Ok(ArchiveInstallReport { files, bytes })
}

fn replace_current_tree(state_dir: &Path, staged_current: &Path) -> Result<(), String> {
    let current = state_dir.join("current");
    let previous = state_dir.join(".previous-current");
    if previous.exists() {
        std::fs::remove_dir_all(&previous)
            .map_err(|error| format!("failed to remove previous staging backup: {error}"))?;
    }
    if current.exists() {
        std::fs::rename(&current, &previous)
            .map_err(|error| format!("failed to move current docs aside: {error}"))?;
    }
    if let Err(error) = std::fs::rename(staged_current, &current) {
        if previous.exists() {
            let _ = std::fs::rename(&previous, &current);
        }
        return Err(format!("failed to promote staged docs: {error}"));
    }
    if previous.exists() {
        std::fs::remove_dir_all(previous)
            .map_err(|error| format!("failed to remove previous docs tree: {error}"))?;
    }
    Ok(())
}

fn safe_archive_path(path: &Path) -> Result<PathBuf, String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("unsafe archive path '{}'", path.display()));
            }
        }
    }
    Ok(relative)
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
