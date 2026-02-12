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

fn print_usage() {
    eprintln!("ndx — MCP File Index Server");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  ndx [path]       Start MCP server for the given project root (default: .)");
    eprintln!("  ndx init [path]  Create .mcp.json for Claude Code in the given directory (default: .)");
    eprintln!("  ndx help         Show this help message");
}

fn cmd_init(dir: PathBuf) -> Result<()> {
    let dir = dir.canonicalize().context("invalid directory path")?;
    let mcp_path = dir.join(".mcp.json");
    let ndx_bin = std::env::current_exe().context("failed to resolve ndx binary path")?;
    let ndx_bin_str = ndx_bin.to_string_lossy();

    // Merge into existing .mcp.json if present
    let mut config: serde_json::Value = if mcp_path.exists() {
        let data = std::fs::read_to_string(&mcp_path).context("failed to read .mcp.json")?;
        serde_json::from_str(&data).context("failed to parse .mcp.json")?
    } else {
        serde_json::json!({})
    };

    let servers = config
        .as_object_mut()
        .context("expected .mcp.json to be an object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    servers
        .as_object_mut()
        .context("expected mcpServers to be an object")?
        .insert(
            "ndx".to_string(),
            serde_json::json!({
                "command": ndx_bin_str,
                "args": ["."]
            }),
        );

    let output = serde_json::to_string_pretty(&config)?;
    std::fs::write(&mcp_path, output)?;
    eprintln!("Created {}", mcp_path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Handle init and help before starting the server
    match args.first().map(|s| s.as_str()) {
        Some("init") => {
            let dir = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            return cmd_init(dir);
        }
        Some("help" | "--help" | "-h") => {
            print_usage();
            return Ok(());
        }
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ndx=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let root = match args.first() {
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
