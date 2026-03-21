use crate::index::{FileEntry, Index};
use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::UNIX_EPOCH;

fn build_gitignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists() {
        builder.add(&gitignore_path);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn is_ignored(gitignore: &Gitignore, path: &Path, is_dir: bool) -> bool {
    gitignore.matched(path, is_dir).is_ignore()
}

/// Classified event kind for debounced processing.
#[derive(Debug, Clone, Copy)]
enum DebouncedKind {
    Upsert,
    Remove,
}

pub fn start_watcher(index: Arc<Index>) -> Result<()> {
    let root = index.root().to_path_buf();
    let gitignore = Arc::new(RwLock::new(build_gitignore(&root)));
    let ndx_dir = root.join(".ndx");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let gi = gitignore.clone();
    tokio::spawn(async move {
        let _watcher = watcher; // keep alive
        let debounce_ms = 200u64;

        loop {
            // Wait for first event
            let first = match rx.recv().await {
                Some(event) => event,
                None => break, // channel closed
            };

            // Collect events for debounce window
            let mut raw_events = vec![first];
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_millis(debounce_ms);

            loop {
                tokio::select! {
                    event = rx.recv() => {
                        match event {
                            Some(e) => raw_events.push(e),
                            None => break, // channel closed
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        break;
                    }
                }
            }

            // Deduplicate by path, keeping latest event kind
            let mut deduped: HashMap<PathBuf, DebouncedKind> = HashMap::new();
            let mut gitignore_changed = false;

            for event_result in &raw_events {
                let event = match event_result {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("watch error: {}", e);
                        continue;
                    }
                };

                for path in &event.paths {
                    if path.starts_with(&ndx_dir) {
                        continue;
                    }

                    if path.file_name().map(|n| n == ".gitignore").unwrap_or(false) {
                        gitignore_changed = true;
                    }

                    let is_dir = path.is_dir();
                    if is_ignored(&gi.read().unwrap(), path, is_dir) {
                        continue;
                    }
                    if is_dir {
                        continue;
                    }

                    let kind = match event.kind {
                        EventKind::Remove(_) => DebouncedKind::Remove,
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            // If file was created then deleted within window, check existence
                            if !path.exists() {
                                DebouncedKind::Remove
                            } else {
                                DebouncedKind::Upsert
                            }
                        }
                        _ => continue,
                    };

                    deduped.insert(path.clone(), kind);
                }
            }

            // Rebuild gitignore if needed
            if gitignore_changed {
                let new_gi = build_gitignore(&root);
                *gi.write().unwrap() = new_gi;
                tracing::debug!("rebuilt gitignore matcher");
            }

            // Classify into upserts and removals
            let mut upserts: Vec<(String, PathBuf)> = Vec::new();
            let mut removals: Vec<String> = Vec::new();

            for (path, kind) in &deduped {
                let rel = match index.rel_path(path) {
                    Some(r) => r,
                    None => continue,
                };
                match kind {
                    DebouncedKind::Upsert => upserts.push((rel, path.clone())),
                    DebouncedKind::Remove => removals.push(rel),
                }
            }

            // Batch removals
            if !removals.is_empty() {
                if let Err(e) = index.remove_files_batch(&removals) {
                    tracing::warn!("batch remove failed: {}", e);
                }
                tracing::debug!("watcher: removed {} files", removals.len());
            }

            // Batch upserts: metadata + content
            if !upserts.is_empty() {
                let mut meta_batch: Vec<(String, FileEntry)> = Vec::new();
                let mut content_batch: Vec<(String, Vec<u8>)> = Vec::new();
                let mut hash_batch: Vec<(String, u64, u64)> = Vec::new();

                for (rel, abs_path) in &upserts {
                    let metadata = match std::fs::metadata(abs_path) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let mtime = metadata
                        .modified()
                        .unwrap_or(UNIX_EPOCH)
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let size = metadata.len();

                    meta_batch.push((
                        rel.clone(),
                        FileEntry {
                            size,
                            modified: mtime,
                            is_dir: false,
                        },
                    ));
                    hash_batch.push((rel.clone(), mtime, size));

                    match std::fs::read(abs_path) {
                        Ok(content) => content_batch.push((rel.clone(), content)),
                        Err(e) => tracing::warn!("read failed for {}: {}", rel, e),
                    }
                }

                if let Err(e) = index.upsert_batch(&meta_batch) {
                    tracing::warn!("batch upsert failed: {}", e);
                }
                if let Err(e) = index.index_content_batch(&content_batch) {
                    tracing::warn!("batch content index failed: {}", e);
                }
                if let Err(e) = index.set_file_hashes_batch(&hash_batch) {
                    tracing::warn!("batch hash update failed: {}", e);
                }

                tracing::debug!("watcher: upserted {} files", upserts.len());
            }
        }
    });

    Ok(())
}
