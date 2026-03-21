use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const MANIFEST_INDEX_URL: &str = "https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands/index.txt";
const MANIFEST_BASE_URL: &str = "https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands";

const SKILL_CONTENT: &str = r#"# ndx — Fast File Index & Memory Search

Use the `ndx` CLI for trigram-accelerated file search, project file listing, session memory queries, and cross-referencing. ndx is available via Bash and works in all contexts including subagents.

## When to use ndx

- **Content search across many files** — faster than grep for large codebases due to trigram index
- **Session memory** — find what was discussed or done in previous Claude Code sessions
- **Cross-referencing** — find which sessions touched a file, or what files a session modified

## File Index Commands

All file index commands operate on the project in the current working directory. The first invocation scans and indexes the project (~100ms for 10K files).

### Search file contents
```bash
ndx search <pattern>
ndx search "TODO" --file-pattern "*.rs"
ndx search "fn main" -B 2 -A 5
ndx search "error" --output files        # just file names
ndx search "import" --output count       # match counts per file
ndx search "pattern" --max-results 50 --offset 100  # pagination
```

### List files
```bash
ndx list                                  # all indexed files
ndx list --path src/                      # files under src/
ndx list --pattern "*.rs"                 # filter by glob
ndx list --sort modified                  # newest first
```

### Find files by glob
```bash
ndx find "**/*.toml"
ndx find "src/**/*.rs" --sort modified
```

### Index status
```bash
ndx status
```

## Memory Commands

Search and browse past Claude Code session transcripts and command events.

```bash
ndx memory search "database migration"          # search session transcripts
ndx memory events "docker"                       # search command event log
ndx memory list                                  # recent sessions
ndx memory list --project /path/to/project       # filter by project
ndx memory stats                                 # session/event/agent counts
ndx memory session <session-id>                  # full session details
ndx memory context                               # recent sessions + events for current project
ndx memory context --project /path/to/project    # for a specific project
ndx memory subagents "search query"              # search subagent transcripts
ndx memory subagents "query" --parent <id>       # filter by parent session
ndx memory tree <session-id>                     # session + subagent tree
```

All memory commands accept `--limit N` to control result count.

## Cross-Reference Commands

```bash
ndx xref file src/main.rs                # find sessions that touched this file
ndx xref session <session-id>            # list files touched by a session
```

## Maintenance

```bash
ndx scan              # re-scan project index + memory database
ndx install           # download command manifests, register hook + skill
ndx init              # install ndx skill into current project
```
"#;

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

    // 3. Register hook in ~/.claude/settings.json (no MCP server)
    let settings_path = home.join(".claude").join("settings.json");
    register_claude_settings(&settings_path, &ndx_bin_str)?;
    eprintln!("  Hook: PreToolUse Bash hook registered in {}", settings_path.display());

    // 4. Install global skill
    let skill_dir = home.join(".claude").join("commands");
    install_skill(&skill_dir)?;
    eprintln!("  Skill: installed to {}/ndx.md", skill_dir.display());

    eprintln!();
    eprintln!("ndx install complete");
    eprintln!("  Restart Claude Code to activate.");

    Ok(())
}

/// Install the ndx skill into a specific project directory.
pub fn install_skill_to_project(project_dir: &Path) -> Result<()> {
    let skill_dir = project_dir.join(".claude").join("commands");
    install_skill(&skill_dir)
}

fn install_skill(skill_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(skill_dir)?;
    let skill_path = skill_dir.join("ndx.md");
    std::fs::write(&skill_path, SKILL_CONTENT)?;
    Ok(())
}

fn download_manifests(commands_dir: &PathBuf) -> usize {
    // Fetch index.txt with ureq
    let index_body = match ureq::get(MANIFEST_INDEX_URL).call() {
        Ok(resp) => match resp.into_string() {
            Ok(body) => body,
            Err(_) => {
                eprintln!("  Warning: could not read manifest index body");
                return 0;
            }
        },
        Err(e) => {
            eprintln!("  Warning: could not fetch manifest index: {}", e);
            eprintln!("    URL: {}", MANIFEST_INDEX_URL);
            return 0;
        }
    };

    let keys: Vec<String> = index_body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();

    let total = keys.len();
    let downloaded = AtomicUsize::new(0);
    let progress = AtomicUsize::new(0);

    // Parallel download with rayon
    keys.par_iter().for_each(|key| {
        let url = format!("{}/{}.yaml", MANIFEST_BASE_URL, key);
        let dest = commands_dir.join(format!("{}.yaml", key));

        if let Ok(resp) = ureq::get(&url).call() {
            if let Ok(body) = resp.into_string() {
                if std::fs::write(&dest, body).is_ok() {
                    downloaded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
        if done % 50 == 0 || done == total {
            eprint!("\r  Downloading: {}/{}", done, total);
        }
    });
    eprintln!();

    downloaded.load(Ordering::Relaxed)
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

    // Remove ndx MCP server if previously registered
    if let Some(mcp_servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        mcp_servers.remove("ndx");
        // Remove mcpServers key entirely if empty
        if mcp_servers.is_empty() {
            obj.remove("mcpServers");
        }
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
