use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::io::{Cursor, Read, Write};
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
const MAX_RELEASE_FILES: u64 = 20_000;
const RELEASE_SCHEMA_VERSION: u32 = 1;
const EXPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Clone)]
struct AppState {
    state_dir: Arc<PathBuf>,
    admin_token: Option<String>,
}

struct ReleaseInstallReport {
    release_id: String,
    files: u64,
    bytes: u64,
    archive_sha256: String,
    site_sha256: String,
    created: bool,
}

struct PromoteReport {
    previous_release_id: Option<String>,
    current_release_id: String,
}

struct ReleaseMetadata<'a> {
    release_id: &'a str,
    format: &'a str,
    archive_bytes: u64,
    archive_sha256: &'a str,
    files: u64,
    uncompressed_bytes: u64,
    site_sha256: &'a str,
}

enum DocsInstallError {
    InvalidArchive(String),
    LimitExceeded(String),
    NotFound(String),
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
            Self::NotFound(message) => {
                (StatusCode::NOT_FOUND, format!("{prefix}: {message}\n")).into_response()
            }
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
    fs::create_dir_all(state_dir.join("releases")).await?;

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
        .route("/v1/status", get(status))
        .route("/v1/current", post(upload_current_docs))
        .route("/v1/current.tar.gz", post(upload_current_docs_archive))
        .route("/v1/releases", get(list_releases))
        .route(
            "/v1/releases/{release_id}/promote",
            post(promote_release_handler),
        )
        .route("/v1/export", get(export_docs))
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
    let current_release_id = fs::read_to_string(state.state_dir.join("current.txt"))
        .await
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    let release_count = count_release_dirs(&state.state_dir).unwrap_or_default();
    let body = format!(
        "service = \"vapor-docs-server\"\nstate = \"{}\"\ncurrent_index_ready = {}\ncurrent_metadata_ready = {}\ncurrent_release_id = \"{}\"\nrelease_count = {}\nmax_upload_bytes = {}\nmax_extracted_bytes = {}\nmax_release_files = {}\nauth_model = \"admin-token-scaffold\"\nlegacy_current_routes = true\n",
        state.state_dir.display(),
        current_index_ready,
        metadata_ready,
        current_release_id,
        release_count,
        MAX_UPLOAD_BYTES,
        MAX_EXTRACTED_BYTES,
        MAX_RELEASE_FILES
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
        Ok(Ok(report)) => release_upload_response("docs: current docs updated", report),
        Ok(Err(error)) => error.response("docs: failed to install current docs"),
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
        Ok(Ok(report)) => release_upload_response("docs: current docs tree updated", report),
        Ok(Err(error)) => error.response("docs: failed to install archive"),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: archive installer task failed: {error}\n"),
        )
            .into_response(),
    }
}

async fn promote_release_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(release_id): AxumPath<String>,
) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        )
            .into_response();
    }

    let state_dir = (*state.state_dir).clone();
    let result =
        tokio::task::spawn_blocking(move || promote_release(&state_dir, &release_id)).await;
    match result {
        Ok(Ok(report)) => (
            StatusCode::OK,
            format!(
                "docs: promoted release\nprevious_release_id = \"{}\"\ncurrent_release_id = \"{}\"\n",
                report.previous_release_id.unwrap_or_default(),
                report.current_release_id
            ),
        )
            .into_response(),
        Ok(Err(error)) => error.response("docs: failed to promote release"),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: promote task failed: {error}\n"),
        )
            .into_response(),
    }
}

async fn list_releases(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        )
            .into_response();
    }

    match release_ids(&state.state_dir) {
        Ok(ids) => (StatusCode::OK, format!("{}\n", ids.join("\n"))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("docs: failed to list releases: {error}\n"),
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

    let current_release_id = fs::read_to_string(state.state_dir.join("current.txt"))
        .await
        .unwrap_or_default();
    let mut body = format!(
        "# Vapor docs export manifest\n\nschema_version = {}\ncurrent_release_id = \"{}\"\n\n",
        EXPORT_SCHEMA_VERSION,
        current_release_id.trim()
    );
    let ids = release_ids(&state.state_dir).unwrap_or_default();
    for release_id in ids {
        let metadata = fs::read_to_string(
            state
                .state_dir
                .join("releases")
                .join(&release_id)
                .join("release.toml"),
        )
        .await
        .unwrap_or_default();
        body.push_str(&format!("## {release_id}\n{metadata}\n"));
    }

    (StatusCode::OK, body).into_response()
}

fn release_upload_response(prefix: &str, report: ReleaseInstallReport) -> Response {
    (
        StatusCode::CREATED,
        format!(
            "{prefix}\nrelease_id = \"{}\"\ncreated = {}\nfiles = {}\nbytes = {}\narchive_sha256 = \"{}\"\nsite_sha256 = \"{}\"\n",
            report.release_id,
            report.created,
            report.files,
            report.bytes,
            report.archive_sha256,
            report.site_sha256
        ),
    )
        .into_response()
}

fn install_single_file_docs(
    state_dir: &Path,
    body: &[u8],
) -> Result<ReleaseInstallReport, DocsInstallError> {
    std::fs::create_dir_all(state_dir)?;
    let archive_sha256 = sha256_hex(body);
    let release_id = format!("legacy-html-{}", &archive_sha256[..16]);
    let releases_dir = state_dir.join("releases");
    std::fs::create_dir_all(&releases_dir)?;
    let final_release = releases_dir.join(&release_id);
    let mut created = false;

    if !final_release.exists() {
        let staging = tempfile::Builder::new()
            .prefix(".release-upload-")
            .tempdir_in(&releases_dir)
            .map_err(|error| DocsInstallError::Storage(error.to_string()))?;
        let staging_release = staging.path().join("release");
        let staging_site = staging_release.join("site");
        std::fs::create_dir_all(&staging_site)?;
        std::fs::write(staging_site.join("index.html"), body)?;
        let site_sha256 = site_sha256(&staging_site)?;
        write_release_metadata(
            &staging_release,
            ReleaseMetadata {
                release_id: &release_id,
                format: "single-html",
                archive_bytes: body.len() as u64,
                archive_sha256: &archive_sha256,
                files: 1,
                uncompressed_bytes: body.len() as u64,
                site_sha256: &site_sha256,
            },
        )?;
        std::fs::rename(&staging_release, &final_release)?;
        created = true;
    }

    let site_sha256 = read_release_field(&final_release, "site_sha256").unwrap_or_default();
    promote_release(state_dir, &release_id)?;
    Ok(ReleaseInstallReport {
        release_id,
        files: 1,
        bytes: body.len() as u64,
        archive_sha256,
        site_sha256,
        created,
    })
}

fn install_archive_docs(
    state_dir: &Path,
    body: &[u8],
) -> Result<ReleaseInstallReport, DocsInstallError> {
    std::fs::create_dir_all(state_dir)?;
    let archive_sha256 = sha256_hex(body);
    let release_id = format!("legacy-archive-{}", &archive_sha256[..16]);
    let releases_dir = state_dir.join("releases");
    std::fs::create_dir_all(&releases_dir)?;
    let final_release = releases_dir.join(&release_id);
    let mut created = false;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    let mut site_digest = read_release_field(&final_release, "site_sha256").unwrap_or_default();

    if !final_release.exists() {
        let staging = tempfile::Builder::new()
            .prefix(".release-upload-")
            .tempdir_in(&releases_dir)
            .map_err(|error| DocsInstallError::Storage(error.to_string()))?;
        let staging_release = staging.path().join("release");
        let staging_site = staging_release.join("site");
        std::fs::create_dir_all(&staging_site)?;

        let report = extract_archive_to_site(body, &staging_site)?;
        files = report.files;
        bytes = report.bytes;
        site_digest = site_sha256(&staging_site)?;
        write_release_metadata(
            &staging_release,
            ReleaseMetadata {
                release_id: &release_id,
                format: "tar-gzip-tree",
                archive_bytes: body.len() as u64,
                archive_sha256: &archive_sha256,
                files,
                uncompressed_bytes: bytes,
                site_sha256: &site_digest,
            },
        )?;
        std::fs::rename(&staging_release, &final_release)?;
        created = true;
    }

    promote_release(state_dir, &release_id)?;
    Ok(ReleaseInstallReport {
        release_id,
        files,
        bytes,
        archive_sha256,
        site_sha256: site_digest,
        created,
    })
}

fn extract_archive_to_site(
    body: &[u8],
    site_dir: &Path,
) -> Result<ReleaseInstallReport, DocsInstallError> {
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
        let destination = site_dir.join(&relative);
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
            if files + 1 > MAX_RELEASE_FILES {
                return Err(DocsInstallError::LimitExceeded(format!(
                    "docs archive exceeds file limit {MAX_RELEASE_FILES}"
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

    if !site_dir.join("index.html").is_file() {
        return Err(DocsInstallError::InvalidArchive(
            "docs archive must contain index.html at its root".to_string(),
        ));
    }

    Ok(ReleaseInstallReport {
        release_id: String::new(),
        files,
        bytes,
        archive_sha256: String::new(),
        site_sha256: String::new(),
        created: true,
    })
}

fn write_release_metadata(
    release_dir: &Path,
    metadata: ReleaseMetadata<'_>,
) -> Result<(), DocsInstallError> {
    let metadata = format!(
        "schema_version = {}\nrelease_id = \"{}\"\ncreated_at_unix = {}\nformat = \"{}\"\narchive_bytes = {}\narchive_sha256 = \"{}\"\nfiles = {}\nuncompressed_bytes = {}\nsite_sha256 = \"{}\"\n",
        RELEASE_SCHEMA_VERSION,
        metadata.release_id,
        unix_now(),
        metadata.format,
        metadata.archive_bytes,
        metadata.archive_sha256,
        metadata.files,
        metadata.uncompressed_bytes,
        metadata.site_sha256,
    );
    std::fs::write(release_dir.join("release.toml"), metadata)?;
    Ok(())
}

fn promote_release(state_dir: &Path, release_id: &str) -> Result<PromoteReport, DocsInstallError> {
    if !valid_release_id(release_id) {
        return Err(DocsInstallError::InvalidArchive(
            "invalid docs release id".to_string(),
        ));
    }
    let release_dir = state_dir.join("releases").join(release_id);
    let release_site = release_dir.join("site");
    if !release_site.join("index.html").is_file() {
        return Err(DocsInstallError::NotFound(format!(
            "release '{}' does not contain site/index.html",
            release_id
        )));
    }
    if !release_dir.join("release.toml").is_file() {
        return Err(DocsInstallError::InvalidArchive(format!(
            "release '{}' is missing release.toml",
            release_id
        )));
    }

    let previous_release_id = std::fs::read_to_string(state_dir.join("current.txt"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let staging = tempfile::Builder::new()
        .prefix(".current-promote-")
        .tempdir_in(state_dir)
        .map_err(|error| DocsInstallError::Storage(error.to_string()))?;
    let staged_current = staging.path().join("current");
    copy_dir_all(&release_site, &staged_current)?;
    std::fs::copy(
        release_dir.join("release.toml"),
        staged_current.join("metadata.toml"),
    )?;
    replace_current_tree(state_dir, &staged_current)?;

    let current_tmp = state_dir.join(".current.txt.tmp");
    std::fs::write(&current_tmp, format!("{release_id}\n"))?;
    std::fs::rename(current_tmp, state_dir.join("current.txt"))?;

    Ok(PromoteReport {
        previous_release_id,
        current_release_id: release_id.to_string(),
    })
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

fn copy_dir_all(source: &Path, destination: &Path) -> Result<(), DocsInstallError> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_dir_all(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            std::fs::copy(source_path, destination_path)?;
        }
    }
    Ok(())
}

fn site_sha256(site_dir: &Path) -> Result<String, DocsInstallError> {
    let mut files = Vec::new();
    collect_site_files(site_dir, site_dir, &mut files)?;
    files.sort();

    let mut hasher = Sha256::new();
    for relative in files {
        let path = site_dir.join(&relative);
        let mut file = std::fs::File::open(&path)?;
        let metadata = file.metadata()?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(metadata.len().to_string().as_bytes());
        hasher.update(b"\0");
        let mut buffer = [0_u8; 8192];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    Ok(hex_digest(&digest))
}

fn collect_site_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), DocsInstallError> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_site_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| DocsInstallError::Storage(error.to_string()))?
                .to_path_buf();
            files.push(relative);
        }
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

fn valid_release_id(release_id: &str) -> bool {
    let bytes = release_id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 96
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn release_ids(state_dir: &Path) -> std::io::Result<Vec<String>> {
    let releases_dir = state_dir.join("releases");
    if !releases_dir.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(releases_dir)? {
        let entry = entry?;
        if entry.metadata()?.is_dir() {
            let id = entry.file_name().to_string_lossy().to_string();
            if valid_release_id(&id) {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

fn count_release_dirs(state_dir: &Path) -> std::io::Result<usize> {
    Ok(release_ids(state_dir)?.len())
}

fn read_release_field(release_dir: &Path, key: &str) -> Option<String> {
    let metadata = std::fs::read_to_string(release_dir.join("release.toml")).ok()?;
    let prefix = format!("{key} = \"");
    metadata.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .and_then(|tail| tail.strip_suffix('"'))
            .map(str::to_string)
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex_digest(&digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
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
    fn release_ids_are_strict() {
        assert!(valid_release_id("legacy-archive-abc123"));
        assert!(valid_release_id("docs.v1_abc-123"));
        assert!(!valid_release_id(""));
        assert!(!valid_release_id("-starts-bad"));
        assert!(!valid_release_id("ends-bad-"));
        assert!(!valid_release_id("../escape"));
    }

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(safe_archive_path(Path::new("index.html")).is_ok());
        assert!(safe_archive_path(Path::new("docs/guide.html")).is_ok());
        assert!(safe_archive_path(Path::new("../secret")).is_err());
        assert!(safe_archive_path(Path::new("/absolute")).is_err());
    }
}
