use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use flate2::read::GzDecoder;
use std::collections::HashSet;
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
const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_DOC_FILES: u64 = 20_000;

#[derive(Clone)]
struct AppState {
    state_dir: Arc<PathBuf>,
    admin_token: Option<String>,
}

struct InstallReport {
    files: u64,
    bytes: u64,
    format: &'static str,
}

enum DocsInstallError {
    InvalidArchive(String),
    LimitExceeded(String),
    Storage(String),
}

impl DocsInstallError {
    fn response(self, prefix: &str) -> Response {
        match self {
            Self::InvalidArchive(message) => {
                (StatusCode::BAD_REQUEST, format!("{prefix}: {message}\n")).into_response()
            }
            Self::LimitExceeded(message) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("{prefix}: {message}\n"),
            )
                .into_response(),
            Self::Storage(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("{prefix}: {message}\n"),
            )
                .into_response(),
        }
    }
}

impl From<std::io::Error> for DocsInstallError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
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
    let app = build_router(state, current_dir);

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("vapor-docs-server listening on {bind}");
    axum::serve(listener, app).await?;

    Ok(())
}

fn build_router(state: AppState, current_dir: PathBuf) -> Router {
    Router::new()
        .route("/", get(serve_current_docs))
        .route("/index.html", get(serve_current_docs))
        .route("/healthz", get(healthz))
        .route("/status", get(status))
        .route("/current", post(upload_current_docs))
        .route("/current.tar.gz", post(upload_current_docs_archive))
        .route("/export", get(export_docs))
        .fallback_service(ServeDir::new(current_dir).append_index_html_on_directories(true))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn status(State(state): State<AppState>) -> Response {
    let current_index_ready = fs::metadata(state.state_dir.join("current").join("index.html"))
        .await
        .is_ok_and(|metadata| metadata.is_file());
    let metadata_ready = fs::metadata(state.state_dir.join("current").join("metadata.toml"))
        .await
        .is_ok_and(|metadata| metadata.is_file());
    let body = format!(
        "service = \"vapor-docs-server\"\nstate = \"{}\"\ncurrent_index_ready = {}\ncurrent_metadata_ready = {}\nmax_upload_bytes = {}\nmax_extracted_bytes = {}\nmax_doc_files = {}\nauth_model = \"admin-token-scaffold\"\n",
        state.state_dir.display(),
        current_index_ready,
        metadata_ready,
        MAX_UPLOAD_BYTES,
        MAX_EXTRACTED_BYTES,
        MAX_DOC_FILES
    );
    (StatusCode::OK, body).into_response()
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
) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        )
            .into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty docs upload\n".to_string()).into_response();
    }

    let state_dir = (*state.state_dir).clone();
    let body = body.to_vec();
    let result =
        tokio::task::spawn_blocking(move || install_single_file_docs(&state_dir, &body)).await;
    match result {
        Ok(Ok(report)) => install_response("docs: current docs updated", report),
        Ok(Err(error)) => error.response("docs: failed to write current docs"),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: installer task failed: {error}\n"),
        )
            .into_response(),
    }
}

async fn upload_current_docs_archive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        )
            .into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty docs upload\n".to_string()).into_response();
    }

    let state_dir = (*state.state_dir).clone();
    let body = body.to_vec();
    let result = tokio::task::spawn_blocking(move || install_archive_docs(&state_dir, &body)).await;
    match result {
        Ok(Ok(report)) => install_response("docs: current docs tree updated", report),
        Ok(Err(error)) => error.response("docs: failed to install archive"),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: archive installer task failed: {error}\n"),
        )
            .into_response(),
    }
}

async fn export_docs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        )
            .into_response();
    }

    let metadata = state.state_dir.join("current").join("metadata.toml");
    let index = state.state_dir.join("current").join("index.html");
    let metadata = fs::read_to_string(metadata).await.unwrap_or_default();
    let index = fs::read_to_string(index).await.unwrap_or_default();
    let body =
        format!("# Vapor docs export\n\n[metadata]\n{metadata}\n--- index.html ---\n{index}");

    (StatusCode::OK, body).into_response()
}

fn install_response(prefix: &str, report: InstallReport) -> Response {
    (
        StatusCode::CREATED,
        format!(
            "{prefix}\nformat = \"{}\"\nfiles = {}\nbytes = {}\n",
            report.format, report.files, report.bytes
        ),
    )
        .into_response()
}

fn install_single_file_docs(
    state_dir: &Path,
    body: &[u8],
) -> Result<InstallReport, DocsInstallError> {
    let staging = staging_current(state_dir)?;
    std::fs::write(staging.path().join("current").join("index.html"), body)?;
    write_current_metadata(
        staging.path().join("current").as_path(),
        "single-html",
        1,
        body.len() as u64,
    )?;
    replace_current_tree(state_dir, &staging.path().join("current"))?;
    Ok(InstallReport {
        files: 1,
        bytes: body.len() as u64,
        format: "single-html",
    })
}

fn install_archive_docs(state_dir: &Path, body: &[u8]) -> Result<InstallReport, DocsInstallError> {
    let staging = staging_current(state_dir)?;
    let staging_current = staging.path().join("current");
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(body)));
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut seen_paths = HashSet::new();
    let entries = archive.entries().map_err(|error| {
        DocsInstallError::InvalidArchive(format!("failed to read archive entries: {error}"))
    })?;

    for entry in entries {
        let mut entry = entry.map_err(|error| {
            DocsInstallError::InvalidArchive(format!("failed to read archive entry: {error}"))
        })?;
        let entry_path = entry.path().map_err(|error| {
            DocsInstallError::InvalidArchive(format!("failed to read archive path: {error}"))
        })?;
        let relative = safe_archive_path(&entry_path)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = staging_current.join(&relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            std::fs::create_dir_all(&destination)?;
        } else if entry_type.is_file() {
            if !seen_paths.insert(relative.clone()) {
                return Err(DocsInstallError::InvalidArchive(format!(
                    "duplicate archive path '{}'",
                    relative.display()
                )));
            }
            let declared_size = entry.header().size().map_err(|error| {
                DocsInstallError::InvalidArchive(format!("failed to read entry size: {error}"))
            })?;
            if files + 1 > MAX_DOC_FILES {
                return Err(DocsInstallError::LimitExceeded(format!(
                    "docs archive exceeds file limit {MAX_DOC_FILES}"
                )));
            }
            if bytes.saturating_add(declared_size) > MAX_EXTRACTED_BYTES {
                return Err(DocsInstallError::LimitExceeded(format!(
                    "docs archive exceeds extracted byte limit {MAX_EXTRACTED_BYTES}"
                )));
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut output = std::fs::File::create(&destination)?;
            let copied = std::io::copy(&mut entry, &mut output)?;
            output.flush()?;
            files += 1;
            bytes = bytes.saturating_add(copied);
        } else {
            return Err(DocsInstallError::InvalidArchive(format!(
                "unsupported archive entry type for '{}'",
                entry_path.display()
            )));
        }
    }

    if !staging_current.join("index.html").is_file() {
        return Err(DocsInstallError::InvalidArchive(
            "docs archive must contain index.html at its root".to_string(),
        ));
    }

    write_current_metadata(&staging_current, "tar-gzip-tree", files, bytes)?;
    replace_current_tree(state_dir, &staging_current)?;

    Ok(InstallReport {
        files,
        bytes,
        format: "tar-gzip-tree",
    })
}

fn staging_current(state_dir: &Path) -> Result<tempfile::TempDir, DocsInstallError> {
    std::fs::create_dir_all(state_dir)?;
    let staging = tempfile::Builder::new()
        .prefix(".current-upload-")
        .tempdir_in(state_dir)
        .map_err(|error| DocsInstallError::Storage(error.to_string()))?;
    std::fs::create_dir_all(staging.path().join("current"))?;
    Ok(staging)
}

fn write_current_metadata(
    current_dir: &Path,
    format: &str,
    files: u64,
    bytes: u64,
) -> Result<(), DocsInstallError> {
    let metadata = format!(
        "published_at_unix = {}\nformat = \"{}\"\nfiles = {}\nbytes = {}\n",
        unix_now(),
        format,
        files,
        bytes
    );
    std::fs::write(current_dir.join("metadata.toml"), metadata)?;
    Ok(())
}

fn replace_current_tree(state_dir: &Path, staged_current: &Path) -> Result<(), DocsInstallError> {
    let current = state_dir.join("current");
    let previous = state_dir.join(".previous-current");
    if previous.exists() {
        std::fs::remove_dir_all(&previous)?;
    }
    if current.exists() {
        std::fs::rename(&current, &previous)?;
    }
    if let Err(error) = std::fs::rename(staged_current, &current) {
        if previous.exists() {
            let _ = std::fs::rename(&previous, &current);
        }
        return Err(DocsInstallError::Storage(format!(
            "failed to promote staged docs: {error}"
        )));
    }
    if previous.exists() {
        std::fs::remove_dir_all(previous)?;
    }
    Ok(())
}

fn safe_archive_path(path: &Path) -> Result<PathBuf, DocsInstallError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DocsInstallError::InvalidArchive(format!(
                    "unsafe archive path '{}'",
                    path.display()
                )));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(safe_archive_path(Path::new("index.html")).is_ok());
        assert!(safe_archive_path(Path::new("docs/guide.html")).is_ok());
        assert!(safe_archive_path(Path::new("../secret")).is_err());
        assert!(safe_archive_path(Path::new("/absolute")).is_err());
    }
}
