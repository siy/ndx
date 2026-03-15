mod hook;
mod index;
mod install;
mod memory;
mod scanner;
mod server;
mod trigram;
mod watcher;

use anyhow::{Context, Result};
use index::Index;
use memory::MemoryIndex;
use rmcp::ServiceExt;
use server::NdxServer;
use std::path::PathBuf;
use std::sync::Arc;

fn print_usage() {
    eprintln!("ndx — MCP File Index Server with Episodic Memory & Command Hooks");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  ndx [path]       Start MCP server for the given project root (default: .)");
    eprintln!("  ndx init [path]  Create .mcp.json for Claude Code in the given directory");
    eprintln!("  ndx hook         PreToolUse hook handler (reads stdin, writes stdout)");
    eprintln!("  ndx filter <key> Output noise filter (reads stdin, writes stdout)");
    eprintln!("  ndx scan         Scan sessions, events, and agents");
    eprintln!("  ndx install      Download manifests, register MCP server + hook");
    eprintln!("  ndx help         Show this help message");
}

fn cmd_init(dir: PathBuf) -> Result<()> {
    let dir = dir.canonicalize().context("invalid directory path")?;
    let mcp_path = dir.join(".mcp.json");
    let ndx_bin = std::env::current_exe().context("failed to resolve ndx binary path")?;
    let ndx_bin_str = ndx_bin.to_string_lossy();

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

fn cmd_hook() -> Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;

    match hook::handle_hook(&input) {
        Ok(Some(response)) => {
            println!("{}", serde_json::to_string(&response)?);
        }
        Ok(None) => {
            // No match — empty output
        }
        Err(e) => {
            eprintln!("[ndx hook] error: {}", e);
        }
    }

    // Phase C: log event directly to memory (best-effort)
    if let Ok(hook_input) = serde_json::from_str::<hook::HookInput>(&input) {
        if let Some(command) = hook_input.tool_input.as_ref().and_then(|ti| ti.command.as_deref()) {
            let parsed = hook::parser::parse_command(command);
            let manifest_key = parsed.as_ref().map(|p| p.key.clone());

            if let Ok(mem) = MemoryIndex::open() {
                let entry = memory::EventEntry {
                    event_ts: chrono::Utc::now().to_rfc3339(),
                    session_id: hook_input.session_id.unwrap_or_default(),
                    project_dir: hook_input.cwd.unwrap_or_default(),
                    tool: "Bash".to_string(),
                    command: command[..command.len().min(500)].to_string(),
                    manifest_key,
                    ingested_at: chrono::Utc::now().to_rfc3339(),
                };
                let _ = mem.insert_event(&entry);
            }
        }
    }

    Ok(())
}

fn cmd_filter(key: &str) -> Result<()> {
    hook::filter::run_filter(key)?;
    Ok(())
}

fn cmd_scan() -> Result<()> {
    let mem = MemoryIndex::open()?;

    let sessions = memory::session::scan_sessions(&mem)?;
    let agents = memory::agent::scan_agents(&mem)?;
    let events = memory::event::ingest_events(&mem)?;

    eprintln!("ndx scan complete");
    eprintln!(
        "  Sessions: {} indexed, {} unchanged, {} errors",
        sessions.indexed, sessions.unchanged, sessions.errors
    );
    eprintln!(
        "  Agents:   {} indexed, {} unchanged",
        agents.indexed, agents.unchanged
    );
    eprintln!("  Events:   {} new", events.new_events);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

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
        Some("hook") => {
            return cmd_hook();
        }
        Some("filter") => {
            let key = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");
            return cmd_filter(key);
        }
        Some("scan") => {
            return cmd_scan();
        }
        Some("install") => {
            return install::run_install();
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

    // Open global memory database
    let mem = match MemoryIndex::open() {
        Ok(m) => {
            // Run initial memory scan
            if let Err(e) = memory::session::scan_sessions(&m) {
                tracing::warn!("session scan failed: {}", e);
            }
            if let Err(e) = memory::agent::scan_agents(&m) {
                tracing::warn!("agent scan failed: {}", e);
            }
            if let Err(e) = memory::event::ingest_events(&m) {
                tracing::warn!("event ingestion failed: {}", e);
            }
            tracing::info!("memory index loaded");
            Some(Arc::new(m))
        }
        Err(e) => {
            tracing::warn!("failed to open memory database: {}", e);
            None
        }
    };

    let server = NdxServer::new(index, mem);
    let service = server
        .serve(rmcp::transport::io::stdio())
        .await
        .context("failed to start MCP server")?;

    tracing::info!("MCP server running on stdio");
    service.waiting().await?;

    Ok(())
}
