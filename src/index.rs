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
        let mut tri_map: HashMap<[u8; 3], Vec<u32>> = HashMap::new();
        let mut doc_entries: Vec<(u32, &str)> = Vec::new();

        for (path, content) in files {
            let doc_id = self.alloc_doc_id();
            for tri in trigram::extract_trigrams(content) {
                tri_map.entry(tri).or_default().push(doc_id);
            }
            doc_entries.push((doc_id, path.as_str()));
        }

        for ids in tri_map.values_mut() {
            ids.sort_unstable();
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

            for (tri, new_ids) in &tri_map {
                let encoded = match tri_table.get(tri.as_slice())? {
                    Some(existing) => {
                        let mut ids = trigram::decode_posting_list(existing.value());
                        ids.extend_from_slice(new_ids);
                        ids.sort_unstable();
                        trigram::encode_posting_list(&ids)
                    }
                    None => trigram::encode_posting_list(new_ids),
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

        let tris = trigram::extract_trigrams(content);
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

            for tri in &tris {
                let mut ids = match tri_table.get(tri.as_slice())? {
                    Some(data) => trigram::decode_posting_list(data.value()),
                    None => Vec::new(),
                };
                if let Err(pos) = ids.binary_search(&doc_id) {
                    ids.insert(pos, doc_id);
                }
                let encoded = trigram::encode_posting_list(&ids);
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

    /// Search trigram index for candidate file paths matching a literal query.
    /// Returns None if query is too short for trigram lookup (caller falls back).
    /// Returns Some(vec) with candidate paths (may be empty = no matches).
    pub fn search_trigram_candidates(&self, query: &str) -> Result<Option<Vec<String>>> {
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

        // Resolve doc_ids to paths, filtering stale (tombstoned) entries
        let mut paths = Vec::new();
        for doc_id in candidates {
            if let Some(path) = doc_paths_table.get(doc_id)? {
                paths.push(path.value().to_string());
            }
        }

        Ok(Some(paths))
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
}
