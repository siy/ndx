use super::MemoryIndex;
use crate::index::Index;
use anyhow::Result;
use std::path::Path;

pub struct FileStatus {
    pub path: String,
    pub status: String,
    pub size: Option<u64>,
    pub modified: Option<String>,
}

pub fn files_for_session_with_status(
    memory: &MemoryIndex,
    index: &Index,
    session_id: &str,
) -> Result<Vec<FileStatus>> {
    let files = memory.files_for_session(session_id)?;
    let mut results = Vec::new();

    for file_path in files {
        let abs_path = if Path::new(&file_path).is_absolute() {
            std::path::PathBuf::from(&file_path)
        } else {
            index.abs_path(&file_path)
        };

        if abs_path.exists() {
            let meta = std::fs::metadata(&abs_path).ok();
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH).ok()
                })
                .map(|d| {
                    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                        .unwrap_or_default()
                });

            results.push(FileStatus {
                path: file_path,
                status: "exists".to_string(),
                size: meta.map(|m| m.len()),
                modified,
            });
        } else {
            results.push(FileStatus {
                path: file_path,
                status: "deleted".to_string(),
                size: None,
                modified: None,
            });
        }
    }

    Ok(results)
}
