use crate::index::{FileEntry, Index};
use crate::trigram;
use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

pub fn scan(index: &Index) -> Result<u64> {
    // Step 1: Parallel filesystem walk
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

                if path.starts_with(ndx_dir) {
                    return if path == ndx_dir.as_path() {
                        ignore::WalkState::Skip
                    } else {
                        ignore::WalkState::Continue
                    };
                }

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
    let total = all_entries.len();

    // Step 2: Incremental diff
    let existing_hashes = index.get_all_file_hashes()?;
    let existing_paths = index.get_all_indexed_paths()?;

    // Files on disk as a set
    let disk_paths: std::collections::HashSet<&str> =
        all_entries.iter().map(|(p, _)| p.as_str()).collect();

    // Removed: in DB but not on disk
    let removed: Vec<String> = existing_paths
        .iter()
        .filter(|p| !disk_paths.contains(p.as_str()))
        .cloned()
        .collect();
    if !removed.is_empty() {
        index.remove_files_batch(&removed)?;
    }

    // Changed: new files or mtime/size differs
    let mut changed: Vec<&(String, FileEntry)> = Vec::new();
    let mut unchanged = 0u64;

    for entry in &all_entries {
        let (path, fe) = entry;
        match existing_hashes.get(path.as_str()) {
            Some(&(old_mtime, old_size)) if old_mtime == fe.modified && old_size == fe.size => {
                unchanged += 1;
            }
            _ => {
                changed.push(entry);
            }
        }
    }

    // Step 3: Batch upsert metadata for changed files
    for chunk in changed.chunks(1000) {
        let batch: Vec<(String, FileEntry)> = chunk.iter().map(|&(ref p, ref e)| (p.clone(), e.clone())).collect();
        index.upsert_batch(&batch)?;
    }

    // Step 4: Parallel trigram extraction via rayon, then sequential DB writes
    let mut reindexed = 0u64;
    for chunk in changed.chunks(500) {
        let precomputed: Vec<(String, HashMap<[u8; 3], Vec<u32>>)> = chunk
            .par_iter()
            .filter_map(|&(ref path, _)| {
                let abs_path = index.abs_path(path);
                let content = std::fs::read(&abs_path).ok()?;
                if trigram::is_binary(&content) {
                    return None;
                }
                let trigrams = trigram::extract_trigrams_with_lines(&content);
                Some((path.clone(), trigrams))
            })
            .collect();

        reindexed += precomputed.len() as u64;
        index.index_content_batch_precomputed(&precomputed)?;
    }

    // Step 5: Update FILE_HASHES for all changed files
    for chunk in changed.chunks(1000) {
        let hash_entries: Vec<(String, u64, u64)> = chunk
            .iter()
            .map(|&(ref p, ref e)| (p.clone(), e.modified, e.size))
            .collect();
        index.set_file_hashes_batch(&hash_entries)?;
    }

    // Step 6: Persist scan state
    index.save_scan_state()?;

    tracing::info!(
        "{} files, {} unchanged, {} re-indexed, {} removed",
        total,
        unchanged,
        reindexed,
        removed.len()
    );

    Ok(total as u64)
}
