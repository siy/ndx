use crate::trigram;
use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

const FILES: TableDefinition<&str, &[u8]> = TableDefinition::new("files");
const TRIGRAMS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("trigrams");
const DOC_PATHS: TableDefinition<u32, &str> = TableDefinition::new("doc_paths");
const PATH_IDS: TableDefinition<&str, u32> = TableDefinition::new("path_ids");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub size: u64,
    pub modified: u64,
    pub is_dir: bool,
}

pub struct Index {
    db: Database,
    root: PathBuf,
    next_doc_id: AtomicU32,
}

impl Index {
    pub fn open(root: PathBuf) -> Result<Self> {
        let db_dir = root.join(".ndx");
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join("index.redb");
        let db = Database::create(&db_path).context("failed to open index database")?;
        let txn = db.begin_write()?;
        txn.open_table(FILES)?;
        txn.open_table(TRIGRAMS)?;
        txn.open_table(DOC_PATHS)?;
        txn.open_table(PATH_IDS)?;
        txn.commit()?;
        Ok(Self {
            db,
            root,
            next_doc_id: AtomicU32::new(0),
        })
    }

    fn alloc_doc_id(&self) -> u32 {
        self.next_doc_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rel_path(&self, abs: &Path) -> Option<String> {
        abs.strip_prefix(&self.root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    pub fn abs_path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    // ── File metadata operations ──

    pub fn upsert(&self, rel_path: &str, entry: &FileEntry) -> Result<()> {
        let data = serde_json::to_vec(entry)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(FILES)?;
            table.insert(rel_path, data.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn upsert_batch(&self, entries: &[(String, FileEntry)]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(FILES)?;
            for (path, entry) in entries {
                let data = serde_json::to_vec(entry)?;
                table.insert(path.as_str(), data.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn remove(&self, rel_path: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(FILES)?;
            table.remove(rel_path)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.next_doc_id.store(0, Ordering::Relaxed);
        let txn = self.db.begin_write()?;
        txn.delete_table(FILES)?;
        txn.delete_table(TRIGRAMS)?;
        txn.delete_table(DOC_PATHS)?;
        txn.delete_table(PATH_IDS)?;
        txn.open_table(FILES)?;
        txn.open_table(TRIGRAMS)?;
        txn.open_table(DOC_PATHS)?;
        txn.open_table(PATH_IDS)?;
        txn.commit()?;
        Ok(())
    }

    pub fn list_all(&self) -> Result<Vec<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILES)?;
        let mut paths = Vec::new();
        for entry in table.range::<&str>(..)? {
            let (key, _) = entry?;
            paths.push(key.value().to_string());
        }
        Ok(paths)
    }

    pub fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILES)?;
        let mut paths = Vec::new();
        for entry in table.range(prefix..)? {
            let (key, _) = entry?;
            let k = key.value();
            if !k.starts_with(prefix) {
                break;
            }
            paths.push(k.to_string());
        }
        Ok(paths)
    }

    pub fn count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILES)?;
        Ok(table.len()?)
    }

    // ── Trigram content index operations ──

    /// Batch-index file contents (used by scanner). Builds per-chunk trigram map
    /// in memory, then merges into redb in a single transaction.
    pub fn index_content_batch(&self, files: &[(String, Vec<u8>)]) -> Result<()> {
        let mut tri_map: HashMap<[u8; 3], Vec<trigram::PostingEntry>> = HashMap::new();
        let mut doc_entries: Vec<(u32, &str)> = Vec::new();

        for (path, content) in files {
            let doc_id = self.alloc_doc_id();
            for (tri, line_nums) in trigram::extract_trigrams_with_lines(content) {
                let entries = tri_map.entry(tri).or_default();
                for line_num in line_nums {
                    entries.push(trigram::PostingEntry { doc_id, line_num });
                }
            }
            doc_entries.push((doc_id, path.as_str()));
        }

        for entries in tri_map.values_mut() {
            entries.sort_unstable();
        }

        let txn = self.db.begin_write()?;
        {
            let mut tri_table = txn.open_table(TRIGRAMS)?;
            let mut doc_paths = txn.open_table(DOC_PATHS)?;
            let mut path_ids = txn.open_table(PATH_IDS)?;

            for &(doc_id, path) in &doc_entries {
                doc_paths.insert(doc_id, path)?;
                path_ids.insert(path, doc_id)?;
            }

            for (tri, new_entries) in &tri_map {
                let encoded = match tri_table.get(tri.as_slice())? {
                    Some(existing) => {
                        let mut entries = trigram::decode_posting_list(existing.value());
                        entries.extend_from_slice(new_entries);
                        entries.sort_unstable();
                        trigram::encode_posting_list(&entries)
                    }
                    None => trigram::encode_posting_list(new_entries),
                };
                tri_table.insert(tri.as_slice(), encoded.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Index a single file's content (used by watcher on create/modify).
    /// Tombstones old doc_id if file was previously indexed.
    pub fn index_file_content(&self, rel_path: &str, content: &[u8]) -> Result<()> {
        if trigram::is_binary(content) {
            self.remove_content(rel_path)?;
            return Ok(());
        }

        let tri_lines = trigram::extract_trigrams_with_lines(content);
        let doc_id = self.alloc_doc_id();

        let txn = self.db.begin_write()?;
        {
            let mut tri_table = txn.open_table(TRIGRAMS)?;
            let mut doc_paths = txn.open_table(DOC_PATHS)?;
            let mut path_ids = txn.open_table(PATH_IDS)?;

            // Tombstone old doc_id (stale entries remain in posting lists,
            // filtered at query time by checking DOC_PATHS)
            if let Some(old_id) = path_ids.remove(rel_path)? {
                doc_paths.remove(old_id.value())?;
            }

            doc_paths.insert(doc_id, rel_path)?;
            path_ids.insert(rel_path, doc_id)?;

            for (tri, line_nums) in &tri_lines {
                let mut entries = match tri_table.get(tri.as_slice())? {
                    Some(data) => trigram::decode_posting_list(data.value()),
                    None => Vec::new(),
                };
                for &line_num in line_nums {
                    let entry = trigram::PostingEntry { doc_id, line_num };
                    if let Err(pos) = entries.binary_search(&entry) {
                        entries.insert(pos, entry);
                    }
                }
                let encoded = trigram::encode_posting_list(&entries);
                tri_table.insert(tri.as_slice(), encoded.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Remove a file from the trigram index (tombstone approach).
    /// Stale posting list entries are filtered at query time.
    pub fn remove_content(&self, rel_path: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut path_ids = txn.open_table(PATH_IDS)?;
            let mut doc_paths = txn.open_table(DOC_PATHS)?;
            let old_id = path_ids.remove(rel_path)?;
            if let Some(id) = old_id {
                doc_paths.remove(id.value())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Search trigram index for candidate file paths and line numbers.
    /// Returns None if query is too short for trigram lookup (caller falls back).
    /// Returns Some(vec) with (path, line_nums) pairs (may be empty = no matches).
    pub fn search_trigram(&self, query: &str) -> Result<Option<Vec<(String, Vec<u32>)>>> {
        let query_tris = match trigram::query_trigrams(query) {
            Some(tris) => tris,
            None => return Ok(None),
        };

        let txn = self.db.begin_read()?;
        let tri_table = txn.open_table(TRIGRAMS)?;
        let doc_paths_table = txn.open_table(DOC_PATHS)?;

        let mut lists = Vec::with_capacity(query_tris.len());
        for tri in &query_tris {
            match tri_table.get(tri.as_slice())? {
                Some(data) => lists.push(trigram::decode_posting_list(data.value())),
                None => return Ok(Some(Vec::new())), // trigram absent → zero matches
            }
        }

        let candidates = trigram::intersect_posting_lists(&lists);

        // Group by doc_id, resolve paths, filter stale (tombstoned) entries
        let mut path_lines: Vec<(String, Vec<u32>)> = Vec::new();
        let mut current_doc_id: Option<u32> = None;
        let mut current_lines: Vec<u32> = Vec::new();

        for entry in &candidates {
            if current_doc_id == Some(entry.doc_id) {
                current_lines.push(entry.line_num);
            } else {
                if let Some(doc_id) = current_doc_id {
                    if let Some(path) = doc_paths_table.get(doc_id)? {
                        path_lines.push((
                            path.value().to_string(),
                            std::mem::take(&mut current_lines),
                        ));
                    } else {
                        current_lines.clear();
                    }
                }
                current_doc_id = Some(entry.doc_id);
                current_lines.push(entry.line_num);
            }
        }
        if let Some(doc_id) = current_doc_id {
            if let Some(path) = doc_paths_table.get(doc_id)? {
                path_lines.push((path.value().to_string(), current_lines));
            }
        }

        Ok(Some(path_lines))
    }

    pub fn trigram_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRIGRAMS)?;
        Ok(table.len()?)
    }

    pub fn content_indexed_count(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DOC_PATHS)?;
        Ok(table.len()?)
    }

    pub fn list_all_with_meta(&self) -> Result<Vec<(String, FileEntry)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILES)?;
        let mut result = Vec::new();
        for entry in table.range::<&str>(..)? {
            let (key, val) = entry?;
            let fe: FileEntry = serde_json::from_slice(val.value())?;
            result.push((key.value().to_string(), fe));
        }
        Ok(result)
    }

    pub fn list_prefix_with_meta(&self, prefix: &str) -> Result<Vec<(String, FileEntry)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILES)?;
        let mut result = Vec::new();
        for entry in table.range(prefix..)? {
            let (key, val) = entry?;
            let k = key.value();
            if !k.starts_with(prefix) {
                break;
            }
            let fe: FileEntry = serde_json::from_slice(val.value())?;
            result.push((k.to_string(), fe));
        }
        Ok(result)
    }

    /// Returns paths in FILES but not in PATH_IDS (non-content-indexed files).
    /// Skips directories.
    pub fn list_non_content_indexed(&self) -> Result<Vec<String>> {
        let txn = self.db.begin_read()?;
        let files_table = txn.open_table(FILES)?;
        let path_ids_table = txn.open_table(PATH_IDS)?;
        let mut result = Vec::new();
        for entry in files_table.range::<&str>(..)? {
            let (key, val) = entry?;
            let path = key.value();
            let fe: FileEntry = serde_json::from_slice(val.value())?;
            if fe.is_dir {
                continue;
            }
            if path_ids_table.get(path)?.is_none() {
                result.push(path.to_string());
            }
        }
        Ok(result)
    }
}
