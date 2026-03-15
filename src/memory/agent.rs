use super::MemoryIndex;
use anyhow::Result;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub struct AgentScanResult {
    pub indexed: u64,
    pub unchanged: u64,
}

pub fn scan_agents(memory: &MemoryIndex) -> Result<AgentScanResult> {
    let home = dirs::home_dir().unwrap_or_default();
    let projects_dir = home.join(".claude").join("projects");

    if !projects_dir.exists() {
        return Ok(AgentScanResult {
            indexed: 0,
            unchanged: 0,
        });
    }

    let mut result = AgentScanResult {
        indexed: 0,
        unchanged: 0,
    };

    // Walk ~/.claude/projects/<slug>/<session-uuid>/subagents/agent-*.jsonl
    walk_for_agents(&projects_dir, memory, &mut result);

    Ok(result)
}

fn walk_for_agents(dir: &Path, memory: &MemoryIndex, result: &mut AgentScanResult) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().map_or(false, |n| n == "subagents") {
                // Found subagents directory — derive parent_session_id from grandparent dir
                let parent_session_id = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();

                scan_subagent_dir(&path, &parent_session_id, memory, result);
            } else {
                walk_for_agents(&path, memory, result);
            }
        }
    }
}

fn scan_subagent_dir(
    dir: &Path,
    parent_session_id: &str,
    memory: &MemoryIndex,
    result: &mut AgentScanResult,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if !name.starts_with("agent-") || !name.ends_with(".jsonl") {
            continue;
        }

        let agent_id = name
            .strip_prefix("agent-")
            .and_then(|s| s.strip_suffix(".jsonl"))
            .unwrap_or("")
            .to_string();

        if agent_id.is_empty() {
            continue;
        }

        let mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // Check if already indexed
        if let Ok(Some(existing)) = memory.get_session(&agent_id) {
            if existing.source_modified >= mtime {
                result.unchanged += 1;
                continue;
            }
        }

        match super::transcript::parse_agent_session(&path, parent_session_id) {
            Ok(Some(entry)) => {
                if let Err(e) = memory.upsert_agent(&entry) {
                    tracing::warn!("failed to index agent {}: {}", path.display(), e);
                } else {
                    result.indexed += 1;
                }
            }
            Ok(None) => {
                result.unchanged += 1;
            }
            Err(e) => {
                tracing::warn!("failed to parse agent {}: {}", path.display(), e);
            }
        }
    }
}
