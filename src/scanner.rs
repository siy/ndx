use crate::index::{FileEntry, Index};
use crate::trigram;
use anyhow::Result;
use ignore::WalkBuilder;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

pub fn scan(index: &Index) -> Result<u64> {
    index.clear()?;

    let entries: Mutex<Vec<(String, FileEntry)>> = Mutex::new(Vec::new());
    let root = index.root().to_path_buf();
    let ndx_dir = root.join(".ndx");

    WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build_parallel()
        .run(|| {
            let entries = &entries;
            let root = &root;
            let ndx_dir = &ndx_dir;
            Box::new(move |entry_result| {
                let entry = match entry_result {
                    Ok(e) => e,
                    Err(_) => return ignore::WalkState::Continue,
                };
                let path = entry.path();

                // Skip .ndx directory
                if path.starts_with(ndx_dir) {
                    return if path == ndx_dir.as_path() {
                        ignore::WalkState::Skip
                    } else {
                        ignore::WalkState::Continue
                    };
                }

                // Skip directories
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(true) {
                    return ignore::WalkState::Continue;
                }

                if let Ok(rel) = path.strip_prefix(root) {
                    let rel_str = rel.to_string_lossy().into_owned();
                    let metadata = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => return ignore::WalkState::Continue,
                    };
                    let file_entry = FileEntry {
                        size: metadata.len(),
                        modified: metadata
                            .modified()
                            .unwrap_or(UNIX_EPOCH)
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                        is_dir: false,
                    };
                    entries.lock().unwrap().push((rel_str, file_entry));
                }
                ignore::WalkState::Continue
            })
        });

    let all_entries = entries.into_inner().unwrap();
    let count = all_entries.len() as u64;

    // Phase 1: batch-insert file metadata
    for chunk in all_entries.chunks(1000) {
        index.upsert_batch(chunk)?;
    }

    // Phase 2: index file contents for trigram search (chunks of 500)
    let paths: Vec<&str> = all_entries.iter().map(|(p, _)| p.as_str()).collect();
    let mut indexed = 0u64;
    for chunk in paths.chunks(500) {
        let mut batch: Vec<(String, Vec<u8>)> = Vec::new();
        for &path in chunk {
            let abs_path = index.abs_path(path);
            match std::fs::read(&abs_path) {
                Ok(content) => {
                    if !trigram::is_binary(&content) {
                        batch.push((path.to_string(), content));
                    }
                }
                Err(_) => continue,
            }
        }
        indexed += batch.len() as u64;
        index.index_content_batch(&batch)?;
    }

    tracing::info!("{} files content-indexed for trigram search", indexed);
    Ok(count)
}
