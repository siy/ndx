use anyhow::{Context, Result};
use std::path::PathBuf;

const MANIFEST_INDEX_URL: &str = "https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands/index.txt";
const MANIFEST_BASE_URL: &str = "https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands";

pub fn run_install() -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ndx_dir = home.join(".ndx");
    let commands_dir = ndx_dir.join("commands");
    let ndx_bin = std::env::current_exe().context("cannot determine ndx binary path")?;
    let ndx_bin_str = ndx_bin.to_string_lossy().into_owned();

    // 1. Create directories
    std::fs::create_dir_all(&commands_dir)?;
    eprintln!("Created {}", commands_dir.display());

    // 2. Download manifests
    eprintln!("Downloading command manifests from kcp-commands...");
    let manifest_count = download_manifests(&commands_dir);
    eprintln!("  Manifests: {} files in {}", manifest_count, commands_dir.display());

    // 3. Register MCP server + hook in ~/.claude/settings.json
    let settings_path = home.join(".claude").join("settings.json");
    register_claude_settings(&settings_path, &ndx_bin_str)?;
    eprintln!("  MCP server: registered in {}", settings_path.display());
    eprintln!("  Hook: PreToolUse Bash hook registered");

    eprintln!();
    eprintln!("ndx install complete");
    eprintln!("  Restart Claude Code to activate.");

    Ok(())
}

fn download_manifests(commands_dir: &PathBuf) -> usize {
    // Try to fetch index.txt to get list of manifest keys
    let index_result = std::process::Command::new("curl")
        .args(["-fsSL", "--connect-timeout", "10", MANIFEST_INDEX_URL])
        .output();

    let keys: Vec<String> = match index_result {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .collect()
        }
        _ => {
            eprintln!("  Warning: could not fetch manifest index. Try manually:");
            eprintln!("    curl -fsSL {} > /tmp/index.txt", MANIFEST_INDEX_URL);
            return 0;
        }
    };

    let total = keys.len();
    let mut downloaded = 0usize;

    for (i, key) in keys.iter().enumerate() {
        let url = format!("{}/{}.yaml", MANIFEST_BASE_URL, key);
        let dest = commands_dir.join(format!("{}.yaml", key));

        let result = std::process::Command::new("curl")
            .args(["-fsSL", "--connect-timeout", "5", "-o"])
            .arg(&dest)
            .arg(&url)
            .output();

        match result {
            Ok(output) if output.status.success() => {
                downloaded += 1;
            }
            _ => {
                // Skip failures silently
            }
        }

        // Progress indicator every 50
        if (i + 1) % 50 == 0 || i + 1 == total {
            eprint!("\r  Downloading: {}/{}", i + 1, total);
        }
    }
    eprintln!();

    downloaded
}

fn register_claude_settings(settings_path: &PathBuf, ndx_bin: &str) -> Result<()> {
    // Ensure directory exists
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Load existing settings or create new
    let mut settings: serde_json::Value = if settings_path.exists() {
        let data = std::fs::read_to_string(settings_path)?;
        serde_json::from_str(&data).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let obj = settings.as_object_mut().context("settings must be an object")?;

    // Register MCP server
    let mcp_servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(servers) = mcp_servers.as_object_mut() {
        servers.insert(
            "ndx".to_string(),
            serde_json::json!({
                "command": ndx_bin,
                "args": ["."]
            }),
        );
    }

    // Register PreToolUse hook
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(hooks_obj) = hooks.as_object_mut() {
        let pre_tool_use = hooks_obj
            .entry("PreToolUse")
            .or_insert_with(|| serde_json::json!([]));

        if let Some(arr) = pre_tool_use.as_array_mut() {
            // Remove existing kcp-commands or ndx entries
            arr.retain(|entry| {
                let matcher = entry.get("matcher").and_then(|v| v.as_str());
                if matcher != Some("Bash") {
                    return true;
                }
                // Check if it's a kcp or ndx hook
                if let Some(hooks_arr) = entry.get("hooks").and_then(|v| v.as_array()) {
                    for h in hooks_arr {
                        if let Some(cmd) = h.get("command").and_then(|v| v.as_str()) {
                            if cmd.contains("kcp") || cmd.contains("ndx") {
                                return false;
                            }
                        }
                    }
                }
                true
            });

            // Add ndx hook
            arr.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": format!("{} hook", ndx_bin),
                    "timeout": 10,
                    "statusMessage": "ndx: looking up command manifest..."
                }]
            }));
        }
    }

    let output = serde_json::to_string_pretty(&settings)?;
    std::fs::write(settings_path, output)?;

    Ok(())
}
