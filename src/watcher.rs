use crate::index::{FileEntry, Index};
use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{EventKind, RecursiveMode, Watcher};
use std::path::Path;
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
        while let Some(event) = rx.recv().await {
            let event = match event {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("watch error: {}", e);
                    continue;
                }
            };

            for path in &event.paths {
                // Skip .ndx directory
                if path.starts_with(&ndx_dir) {
                    continue;
                }

                // Rebuild gitignore when .gitignore changes
                if path.file_name().map(|n| n == ".gitignore").unwrap_or(false) {
                    let new_gi = build_gitignore(&root);
                    *gi.write().unwrap() = new_gi;
                    tracing::debug!("rebuilt gitignore matcher");
                }

                let is_dir = path.is_dir();

                // Check gitignore
                if is_ignored(&gi.read().unwrap(), path, is_dir) {
                    continue;
                }

                // Skip directories
                if is_dir {
                    continue;
                }

                let rel = match index.rel_path(path) {
                    Some(r) => r,
                    None => continue,
                };

                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        let metadata = match std::fs::metadata(path) {
                            Ok(m) => m,
                            Err(_) => continue,
                        };
                        let entry = FileEntry {
                            size: metadata.len(),
                            modified: metadata
                                .modified()
                                .unwrap_or(UNIX_EPOCH)
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                            is_dir: false,
                        };
                        if let Err(e) = index.upsert(&rel, &entry) {
                            tracing::warn!("index upsert failed for {}: {}", rel, e);
                        }
                        // Update trigram content index
                        match std::fs::read(path) {
                            Ok(content) => {
                                if let Err(e) = index.index_file_content(&rel, &content) {
                                    tracing::warn!("content index failed for {}: {}", rel, e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("read failed for {}: {}", rel, e);
                            }
                        }
                    }
                    EventKind::Remove(_) => {
                        if let Err(e) = index.remove(&rel) {
                            tracing::warn!("index remove failed for {}: {}", rel, e);
                        }
                        if let Err(e) = index.remove_content(&rel) {
                            tracing::warn!("content remove failed for {}: {}", rel, e);
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    Ok(())
}
