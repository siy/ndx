use crate::daemon;
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Send a query to the daemon, auto-starting it if needed.
pub fn query(root: &Path, method: &str, params: serde_json::Value) -> Result<String> {
    let sock = daemon::socket_path(root);

    let stream = if sock.exists() {
        match UnixStream::connect(&sock) {
            Ok(s) => s,
            Err(_) => {
                // Stale socket — clean up and restart
                let _ = std::fs::remove_file(&sock);
                let _ = std::fs::remove_file(daemon::pid_path(root));
                start_daemon(root)?;
                wait_for_socket(&sock)?;
                UnixStream::connect(&sock).context("failed to connect after restart")?
            }
        }
    } else {
        start_daemon(root)?;
        wait_for_socket(&sock)?;
        UnixStream::connect(&sock).context("failed to connect after start")?
    };

    send_request(stream, method, params)
}

/// Send shutdown to the daemon.
pub fn stop(root: &Path) -> Result<()> {
    let sock = daemon::socket_path(root);
    if !sock.exists() {
        eprintln!("no daemon running");
        return Ok(());
    }

    match UnixStream::connect(&sock) {
        Ok(stream) => {
            send_request(stream, "shutdown", serde_json::json!({}))?;
            eprintln!("daemon stopped");
        }
        Err(_) => {
            // Stale socket
            let _ = std::fs::remove_file(&sock);
            let _ = std::fs::remove_file(daemon::pid_path(root));
            eprintln!("cleaned up stale socket");
        }
    }

    Ok(())
}

fn send_request(
    mut stream: UnixStream,
    method: &str,
    params: serde_json::Value,
) -> Result<String> {
    let req = serde_json::json!({"method": method, "params": params});
    writeln!(stream, "{}", serde_json::to_string(&req)?)?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    let resp: serde_json::Value =
        serde_json::from_str(response.trim()).context("invalid daemon response")?;

    if let Some(ok) = resp.get("ok").and_then(|v| v.as_str()) {
        Ok(ok.to_string())
    } else if let Some(err) = resp.get("err").and_then(|v| v.as_str()) {
        anyhow::bail!("{}", err)
    } else {
        anyhow::bail!("invalid daemon response: {}", response)
    }
}

fn start_daemon(root: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve ndx binary")?;
    let log_path = root.join(".ndx").join("ndx.log");
    std::fs::create_dir_all(root.join(".ndx"))?;

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("failed to open daemon log")?;

    std::process::Command::new(exe)
        .arg("daemon")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log_file)
        .spawn()
        .context("failed to start daemon")?;

    Ok(())
}

fn wait_for_socket(sock: &Path) -> Result<()> {
    for _ in 0..100 {
        if sock.exists() {
            // Brief extra wait for the listener to be fully ready
            std::thread::sleep(std::time::Duration::from_millis(50));
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("daemon failed to start (socket not created within 10s)")
}
