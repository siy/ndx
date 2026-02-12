mod index;
mod scanner;
mod server;
mod trigram;
mod watcher;

use anyhow::{Context, Result};
use index::Index;
use rmcp::ServiceExt;
use server::NdxServer;
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ndx=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let root = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p)
            .canonicalize()
            .context("invalid project root path")?,
        None => std::env::current_dir().context("failed to get current directory")?,
    };

    tracing::info!("ndx starting for {}", root.display());

    let index = Arc::new(Index::open(root)?);

    let count = scanner::scan(&index)?;
    tracing::info!("initial scan: {} files indexed", count);

    watcher::start_watcher(index.clone())?;
    tracing::info!("file watcher started");

    let server = NdxServer::new(index);
    let service = server
        .serve(rmcp::transport::io::stdio())
        .await
        .context("failed to start MCP server")?;

    tracing::info!("MCP server running on stdio");
    service.waiting().await?;

    Ok(())
}
