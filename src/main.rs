use std::env;
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "127.0.0.1:7112";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = env::var("VAPOR_DOCS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let app = vapor_docs_server::app_router_from_env().await?;
    let listener = TcpListener::bind(&bind).await?;
    eprintln!("vapor-docs-server listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}
