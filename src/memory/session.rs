use super::MemoryIndex;
use anyhow::Result;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub struct ScanResult {
    pub indexed: u64,
    pub unchanged: u64,
    pub errors: u64,
}

pub fn scan_sessions(memory: &MemoryIndex) -> Result<ScanResult> {
    let home = dirs::home_dir().unwrap_or_default();
    let projects_dir = home.join(".claude").join("projects");

    if !projects_dir.exists() {
        return Ok(ScanResult {
            indexed: 0,
            unchanged: 0,
            errors: 0,
        });
    }

    let mut result = ScanResult {
        indexed: 0,
        unchanged: 0,
        errors: 0,
    };

    // Walk ~/.claude/projects/<slug>/*.jsonl (skip subagents/)
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for slug_entry in entries.flatten() {
            let slug_path = slug_entry.path();
            if !slug_path.is_dir() {
                continue;
            }
            let slug = slug_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            scan_slug_dir(&slug_path, &slug, memory, &mut result);
        }
    }

    Ok(result)
}

fn scan_slug_dir(
    slug_dir: &Path,
    slug: &str,
    memory: &MemoryIndex,
    result: &mut ScanResult,
) {
    let Ok(entries) = std::fs::read_dir(slug_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Skip subagents directory (handled separately)
        if path.is_dir() {
            // Check if this is a session UUID directory containing JSONL files
            if let Ok(session_files) = std::fs::read_dir(&path) {
                for sf in session_files.flatten() {
                    let sf_path = sf.path();
                    if sf_path.is_dir() && sf_path.file_name().map_or(false, |n| n == "subagents") {
                        continue;
                    }
                    if sf_path.extension().map_or(false, |e| e == "jsonl") {
                        scan_single_session(&sf_path, slug, memory, result);
                    }
                }
            }
            continue;
        }

        if path.extension().map_or(false, |e| e == "jsonl") {
            scan_single_session(&path, slug, memory, result);
        }
    }
}

fn scan_single_session(
    path: &Path,
    slug: &str,
    memory: &MemoryIndex,
    result: &mut ScanResult,
) {
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Check if already indexed and unchanged
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    if let Ok(Some(existing)) = memory.get_session(&session_id) {
        if existing.source_modified >= mtime {
            result.unchanged += 1;
            return;
        }
    }

    match super::transcript::parse_claude_session(path, slug) {
        Ok(Some(entry)) => {
            if let Err(e) = memory.upsert_session(&entry) {
                tracing::warn!("failed to index session {}: {}", path.display(), e);
                result.errors += 1;
            } else {
                result.indexed += 1;
            }
        }
        Ok(None) => {
            result.unchanged += 1;
        }
        Err(e) => {
            tracing::warn!("failed to parse session {}: {}", path.display(), e);
            result.errors += 1;
        }
    }
}
