use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_BIND: &str = "127.0.0.1:7112";
const DEFAULT_STATE_DIR: &str = "state/docs";
const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn main() -> io::Result<()> {
    let bind = env::var("VAPOR_DOCS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let state_dir = PathBuf::from(
        env::var("VAPOR_DOCS_STATE").unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string()),
    );
    fs::create_dir_all(state_dir.join("current"))?;

    let listener = TcpListener::bind(&bind)?;
    eprintln!("vapor-docs-server listening on {bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream, &state_dir) {
                    eprintln!("request failed: {error}");
                }
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }

    Ok(())
}

fn handle_connection(stream: &mut TcpStream, state_dir: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let Some(request) = read_request(stream, MAX_UPLOAD_BYTES)? else {
        return Ok(());
    };

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/healthz") => respond_text(stream, "200 OK", "ok\n"),
        ("GET", "/") | ("GET", "/index.html") => serve_current_docs(stream, state_dir),
        ("POST", "/v1/current") => upload_current_docs(stream, state_dir, &request),
        ("GET", "/v1/export") => export_docs(stream, state_dir, &request),
        _ => respond_text(stream, "404 Not Found", "not found\n"),
    }
}

fn serve_current_docs(stream: &mut TcpStream, state_dir: &Path) -> io::Result<()> {
    let index = state_dir.join("current").join("index.html");
    let body = fs::read_to_string(index).unwrap_or_else(|_| {
        "<!doctype html><title>Vapor Docs</title><h1>Vapor Docs</h1><p>No docs have been published yet.</p>\n"
            .to_string()
    });
    respond(
        stream,
        "200 OK",
        "text/html; charset=utf-8",
        body.as_bytes(),
    )
}

fn upload_current_docs(
    stream: &mut TcpStream,
    state_dir: &Path,
    request: &Request,
) -> io::Result<()> {
    if !has_admin_token(request, "VAPOR_DOCS_ADMIN_TOKEN") {
        return respond_text(
            stream,
            "401 Unauthorized",
            "missing or invalid admin token\n",
        );
    }
    if request.body.is_empty() {
        return respond_text(stream, "400 Bad Request", "empty docs upload\n");
    }

    let current = state_dir.join("current");
    fs::create_dir_all(&current)?;
    fs::write(current.join("index.html"), &request.body)?;
    fs::write(
        current.join("metadata.toml"),
        format!(
            "published_at_unix = {}\nbytes = {}\nformat = \"single-html\"\n",
            unix_now(),
            request.body.len()
        ),
    )?;

    respond_text(stream, "201 Created", "docs: current docs updated\n")
}

fn export_docs(stream: &mut TcpStream, state_dir: &Path, request: &Request) -> io::Result<()> {
    if !has_admin_token(request, "VAPOR_DOCS_ADMIN_TOKEN") {
        return respond_text(
            stream,
            "401 Unauthorized",
            "missing or invalid admin token\n",
        );
    }

    let metadata = state_dir.join("current").join("metadata.toml");
    let index = state_dir.join("current").join("index.html");
    let metadata = fs::read_to_string(metadata).unwrap_or_default();
    let index = fs::read_to_string(index).unwrap_or_default();
    let body = format!(
        "# Vapor docs export scaffold\n\n[metadata]\n{metadata}\n--- index.html ---\n{index}"
    );

    respond(
        stream,
        "200 OK",
        "text/plain; charset=utf-8",
        body.as_bytes(),
    )
}

fn has_admin_token(request: &Request, env_name: &str) -> bool {
    let Ok(expected) = env::var(env_name) else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }

    request
        .headers
        .iter()
        .any(|(name, value)| name == "authorization" && value == &format!("Bearer {expected}"))
}

fn read_request(stream: &mut TcpStream, max_body: usize) -> io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if header_end.is_none() {
            header_end = find_header_end(&buffer);
            if let Some(end) = header_end {
                let headers = String::from_utf8_lossy(&buffer[..end]);
                content_length = parse_content_length(&headers).unwrap_or(0);
                if content_length > max_body {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request body too large",
                    ));
                }
            }
        }

        if let Some(end) = header_end {
            if buffer.len() >= end + 4 + content_length {
                break;
            }
        }
    }

    let Some(end) = header_end else {
        return Ok(None);
    };

    let header_text = String::from_utf8_lossy(&buffer[..end]);
    let mut lines = header_text.lines();
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut request_parts = request_line.split_whitespace();
    let Some(method) = request_parts.next() else {
        return Ok(None);
    };
    let Some(path) = request_parts.next() else {
        return Ok(None);
    };

    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let body_start = end + 4;
    let body_end = body_start + content_length;
    let body = buffer
        .get(body_start..body_end)
        .map_or_else(Vec::new, ToOwned::to_owned);

    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn respond_text(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    respond(stream, status, "text/plain; charset=utf-8", body.as_bytes())
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
