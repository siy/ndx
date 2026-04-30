use crate::index::Index;
use crate::scanner;
use crate::server;
use crate::watcher;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Notify;

#[derive(Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Serialize)]
struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    err: Option<String>,
}

impl Response {
    fn ok(s: String) -> Self {
        Self {
            ok: Some(s),
            err: None,
        }
    }
    fn err(s: String) -> Self {
        Self {
            ok: None,
            err: Some(s),
        }
    }
}

pub fn socket_path(root: &Path) -> PathBuf {
    root.join(".ndx").join("ndx.sock")
}

pub fn pid_path(root: &Path) -> PathBuf {
    root.join(".ndx").join("ndx.pid")
}

pub async fn run(root: PathBuf) -> Result<()> {
    let ndx_dir = root.join(".ndx");
    std::fs::create_dir_all(&ndx_dir)?;

    let sock = socket_path(&root);
    let pid = pid_path(&root);

    // Clean up stale socket
    let _ = std::fs::remove_file(&sock);

    // Write PID
    std::fs::write(&pid, std::process::id().to_string())?;

    // Open and scan index
    let index = Arc::new(Index::open(root.clone())?);
    let count = scanner::scan(&index)?;
    tracing::info!("indexed {} files", count);

    // Start watcher
    watcher::start_watcher(index.clone())?;
    tracing::info!("file watcher started");

    // Bind listener
    let listener = UnixListener::bind(&sock)?;
    tracing::info!("listening on {}", sock.display());

    // Shutdown signal
    let shutdown = Arc::new(Notify::new());

    // Handle SIGTERM/SIGINT
    let shutdown_signal = shutdown.clone();
    let sock_cleanup = sock.clone();
    let pid_cleanup = pid.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received signal, shutting down");
        let _ = std::fs::remove_file(&sock_cleanup);
        let _ = std::fs::remove_file(&pid_cleanup);
        shutdown_signal.notify_one();
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let index = index.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, index, shutdown).await {
                        tracing::warn!("connection error: {}", e);
                    }
                });
            }
            _ = shutdown.notified() => {
                break;
            }
        }
    }

    // Cleanup
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&pid);
    tracing::info!("daemon stopped");

    Ok(())
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    index: Arc<Index>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let req: Request = serde_json::from_str(line.trim())
        .context("invalid request JSON")?;
    let is_shutdown = req.method == "shutdown";

    let response = tokio::task::spawn_blocking(move || dispatch(req, &index))
        .await
        .unwrap_or_else(|e| Response::err(format!("task failed: {}", e)));

    let json = serde_json::to_string(&response)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.shutdown().await?;

    if is_shutdown {
        shutdown.notify_one();
    }

    Ok(())
}

fn dispatch(req: Request, index: &Index) -> Response {
    match req.method.as_str() {
        "ping" => Response::ok("pong".to_string()),
        "shutdown" => Response::ok("shutting down".to_string()),
        "list_files" => {
            let path = req.params.get("path").and_then(|v| v.as_str());
            let pattern = req.params.get("pattern").and_then(|v| v.as_str());
            let sort = req.params.get("sort").and_then(|v| v.as_str());
            let tokens = req.params.get("tokens").and_then(|v| v.as_bool()).unwrap_or(false);
            let json = req.params.get("json").and_then(|v| v.as_bool()).unwrap_or(false);
            match server::list_files(index, path, pattern, sort, tokens, json) {
                Ok(r) => Response::ok(r),
                Err(e) => Response::err(e),
            }
        }
        "search_files" => {
            let pattern = match req.params.get("pattern").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => return Response::err("missing 'pattern'".to_string()),
            };
            let sort = req.params.get("sort").and_then(|v| v.as_str());
            let tokens = req.params.get("tokens").and_then(|v| v.as_bool()).unwrap_or(false);
            let json = req.params.get("json").and_then(|v| v.as_bool()).unwrap_or(false);
            match server::search_files(index, pattern, sort, tokens, json) {
                Ok(r) => Response::ok(r),
                Err(e) => Response::err(e),
            }
        }
        "search_content" => {
            let pattern = match req.params.get("pattern").and_then(|v| v.as_str()) {
                Some(p) => p,
                None => return Response::err("missing 'pattern'".to_string()),
            };
            let file_pattern = req.params.get("file_pattern").and_then(|v| v.as_str());
            let max_results = req.params.get("max_results").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let before_ctx = req.params.get("before_context").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let after_ctx = req.params.get("after_context").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let output_mode = req.params.get("output_mode").and_then(|v| v.as_str()).unwrap_or("content");
            let offset = req.params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            match server::search_content(index, pattern, file_pattern, max_results, before_ctx, after_ctx, output_mode, offset) {
                Ok(r) => Response::ok(r),
                Err(e) => Response::err(e),
            }
        }
        "index_status" => {
            match server::index_status(index, None) {
                Ok(r) => Response::ok(r),
                Err(e) => Response::err(e),
            }
        }
        _ => Response::err(format!("unknown method: {}", req.method)),
    }
}
