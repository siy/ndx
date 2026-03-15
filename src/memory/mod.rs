pub mod agent;
pub mod event;
pub mod session;
pub mod transcript;
pub mod xref;

use anyhow::{Context, Result};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::trigram;

// ── Table definitions ──

const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const SESSIONS_BY_PROJECT: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions_by_project");
const SESSION_TRIGRAMS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("session_trigrams");
const SESSION_DOC_PATHS: TableDefinition<u32, &str> = TableDefinition::new("session_doc_paths");
const SESSION_PATH_IDS: TableDefinition<&str, u32> = TableDefinition::new("session_path_ids");

const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("events");
const EVENTS_BY_PROJECT: TableDefinition<&str, u64> = TableDefinition::new("events_by_project");
const EVENT_TRIGRAMS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("event_trigrams");
const EVENT_DOC_PATHS: TableDefinition<u32, &str> = TableDefinition::new("event_doc_paths");
const EVENT_PATH_IDS: TableDefinition<&str, u32> = TableDefinition::new("event_path_ids");
const EVENT_CURSOR: TableDefinition<u8, u64> = TableDefinition::new("event_cursor");
const EVENTS_DEDUP: TableDefinition<&str, ()> = TableDefinition::new("events_dedup");

const AGENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("agents");
const AGENTS_BY_PARENT: TableDefinition<&str, &[u8]> = TableDefinition::new("agents_by_parent");
const AGENT_TRIGRAMS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("agent_trigrams");
const AGENT_DOC_PATHS: TableDefinition<u32, &str> = TableDefinition::new("agent_doc_paths");
const AGENT_PATH_IDS: TableDefinition<&str, u32> = TableDefinition::new("agent_path_ids");

const SESSION_FILES_XREF: TableDefinition<&str, &[u8]> = TableDefinition::new("session_files_xref");
const NEXT_DOC_IDS: TableDefinition<&str, u32> = TableDefinition::new("next_doc_ids");
const META: TableDefinition<&str, u32> = TableDefinition::new("meta");

// ── Data models ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_id: String,
    pub project_dir: String,
    pub git_branch: Option<String>,
    pub slug: String,
    pub model: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tool_names: Vec<String>,
    pub files: Vec<String>,
    pub first_message: Option<String>,
    pub all_user_text: String,
    pub scanned_at: String,
    pub source_path: String,
    pub source_modified: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub event_ts: String,
    pub session_id: String,
    pub project_dir: String,
    pub tool: String,
    pub command: String,
    pub manifest_key: Option<String>,
    pub ingested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub agent_id: String,
    pub parent_session_id: String,
    pub agent_slug: Option<String>,
    pub project_dir: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tool_names: Vec<String>,
    pub first_message: Option<String>,
    pub all_user_text: String,
    pub first_seen_at: Option<String>,
    pub last_updated_at: Option<String>,
    pub message_count: u32,
    pub scanned_at: String,
    pub source_path: String,
    pub source_modified: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStats {
    pub session_count: u64,
    pub event_count: u64,
    pub agent_count: u64,
    pub total_turns: u64,
    pub total_tool_calls: u64,
    pub oldest_session: Option<String>,
    pub newest_session: Option<String>,
    pub top_tools: Vec<(String, u64)>,
}

// ── MemoryIndex ──

pub struct MemoryIndex {
    db: Database,
    next_session_doc_id: AtomicU32,
    next_event_doc_id: AtomicU32,
    next_agent_doc_id: AtomicU32,
    next_event_id: AtomicU64,
}

impl MemoryIndex {
    pub fn open() -> Result<Self> {
        let dir = Self::db_dir()?;
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("memory.redb");
        let db = Database::create(&db_path).context("failed to open memory database")?;

        // Initialize all tables
        {
            let txn = db.begin_write()?;
            txn.open_table(SESSIONS)?;
            txn.open_table(SESSIONS_BY_PROJECT)?;
            txn.open_table(SESSION_TRIGRAMS)?;
            txn.open_table(SESSION_DOC_PATHS)?;
            txn.open_table(SESSION_PATH_IDS)?;
            txn.open_table(EVENTS)?;
            txn.open_table(EVENTS_BY_PROJECT)?;
            txn.open_table(EVENT_TRIGRAMS)?;
            txn.open_table(EVENT_DOC_PATHS)?;
            txn.open_table(EVENT_PATH_IDS)?;
            txn.open_table(EVENT_CURSOR)?;
            txn.open_table(EVENTS_DEDUP)?;
            txn.open_table(AGENTS)?;
            txn.open_table(AGENTS_BY_PARENT)?;
            txn.open_table(AGENT_TRIGRAMS)?;
            txn.open_table(AGENT_DOC_PATHS)?;
            txn.open_table(AGENT_PATH_IDS)?;
            txn.open_table(SESSION_FILES_XREF)?;
            txn.open_table(NEXT_DOC_IDS)?;
            txn.open_table(META)?;
            txn.commit()?;
        }

        // Load persisted doc ID counters
        let (sid, eid, aid, next_eid) = {
            let txn = db.begin_read()?;
            let ids = txn.open_table(NEXT_DOC_IDS)?;
            let sid = ids.get("session")?.map(|v| v.value()).unwrap_or(0);
            let eid = ids.get("event")?.map(|v| v.value()).unwrap_or(0);
            let aid = ids.get("agent")?.map(|v| v.value()).unwrap_or(0);

            let events = txn.open_table(EVENTS)?;
            let next_eid = events
                .last()?
                .map(|(k, _)| k.value() + 1)
                .unwrap_or(0);
            (sid, eid, aid, next_eid)
        };

        Ok(Self {
            db,
            next_session_doc_id: AtomicU32::new(sid),
            next_event_doc_id: AtomicU32::new(eid),
            next_agent_doc_id: AtomicU32::new(aid),
            next_event_id: AtomicU64::new(next_eid),
        })
    }

    pub fn db_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(".ndx"))
    }

    fn alloc_session_doc_id(&self) -> u32 {
        self.next_session_doc_id.fetch_add(1, Ordering::Relaxed)
    }
    fn alloc_event_doc_id(&self) -> u32 {
        self.next_event_doc_id.fetch_add(1, Ordering::Relaxed)
    }
    fn alloc_agent_doc_id(&self) -> u32 {
        self.next_agent_doc_id.fetch_add(1, Ordering::Relaxed)
    }
    fn alloc_event_id(&self) -> u64 {
        self.next_event_id.fetch_add(1, Ordering::Relaxed)
    }

    fn persist_doc_ids(&self) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(NEXT_DOC_IDS)?;
            table.insert("session", self.next_session_doc_id.load(Ordering::Relaxed))?;
            table.insert("event", self.next_event_doc_id.load(Ordering::Relaxed))?;
            table.insert("agent", self.next_agent_doc_id.load(Ordering::Relaxed))?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Build trigram postings for a text in a given trigram table namespace.
    fn index_text_trigrams(
        &self,
        text: &str,
        doc_id: u32,
        tri_table: TableDefinition<&[u8], &[u8]>,
        doc_paths_table: TableDefinition<u32, &str>,
        path_ids_table: TableDefinition<&str, u32>,
        id_key: &str,
    ) -> Result<()> {
        let content = text.as_bytes();
        let tri_lines = trigram::extract_trigrams_with_lines(content);

        let txn = self.db.begin_write()?;
        {
            let mut tri_t = txn.open_table(tri_table)?;
            let mut dp = txn.open_table(doc_paths_table)?;
            let mut pi = txn.open_table(path_ids_table)?;

            // Tombstone old doc_id
            if let Some(old_id) = pi.remove(id_key)? {
                dp.remove(old_id.value())?;
            }

            dp.insert(doc_id, id_key)?;
            pi.insert(id_key, doc_id)?;

            for (tri, line_nums) in &tri_lines {
                let mut entries = match tri_t.get(tri.as_slice())? {
                    Some(data) => trigram::decode_posting_list(data.value()),
                    None => Vec::new(),
                };
                for &ln in line_nums {
                    let entry = trigram::PostingEntry { doc_id, line_num: ln };
                    if let Err(pos) = entries.binary_search(&entry) {
                        entries.insert(pos, entry);
                    }
                }
                let encoded = trigram::encode_posting_list(&entries);
                tri_t.insert(tri.as_slice(), encoded.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Search trigram index for matching IDs.
    fn search_trigrams(
        &self,
        query: &str,
        tri_table: TableDefinition<&[u8], &[u8]>,
        doc_paths_table: TableDefinition<u32, &str>,
    ) -> Result<Vec<String>> {
        let query_tris = match trigram::query_trigrams(query) {
            Some(tris) => tris,
            None => {
                // Query too short, return all IDs
                let txn = self.db.begin_read()?;
                let dp = txn.open_table(doc_paths_table)?;
                let mut ids = Vec::new();
                for entry in dp.range::<u32>(..)? {
                    let (_, v) = entry?;
                    ids.push(v.value().to_string());
                }
                return Ok(ids);
            }
        };

        let txn = self.db.begin_read()?;
        let tri_t = txn.open_table(tri_table)?;
        let dp = txn.open_table(doc_paths_table)?;

        let mut lists = Vec::with_capacity(query_tris.len());
        for tri in &query_tris {
            match tri_t.get(tri.as_slice())? {
                Some(data) => lists.push(trigram::decode_posting_list(data.value())),
                None => return Ok(Vec::new()),
            }
        }

        let candidates = trigram::intersect_posting_lists(&lists);

        // Deduplicate doc IDs and resolve to string keys
        let mut seen = std::collections::HashSet::new();
        let mut ids = Vec::new();
        for entry in &candidates {
            if seen.insert(entry.doc_id) {
                if let Some(path) = dp.get(entry.doc_id)? {
                    ids.push(path.value().to_string());
                }
            }
        }
        Ok(ids)
    }

    // ── Session operations ──

    pub fn upsert_session(&self, entry: &SessionEntry) -> Result<()> {
        let data = serde_json::to_vec(entry)?;
        let project_key = format!("{}\0{}\0{}", entry.project_dir, entry.started_at.as_deref().unwrap_or(""), &entry.session_id);

        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            let mut by_project = txn.open_table(SESSIONS_BY_PROJECT)?;

            // Remove old project index entry if session existed
            if let Some(old_data) = sessions.get(entry.session_id.as_str())? {
                if let Ok(old) = serde_json::from_slice::<SessionEntry>(old_data.value()) {
                    let old_key = format!("{}\0{}\0{}", old.project_dir, old.started_at.as_deref().unwrap_or(""), &old.session_id);
                    by_project.remove(old_key.as_str())?;
                }
            }

            sessions.insert(entry.session_id.as_str(), data.as_slice())?;
            by_project.insert(project_key.as_str(), b"".as_slice())?;
        }
        txn.commit()?;

        // Update trigram index
        let search_text = format!(
            "{} {}",
            entry.first_message.as_deref().unwrap_or(""),
            &entry.all_user_text
        );
        if search_text.len() >= 3 {
            let doc_id = self.alloc_session_doc_id();
            self.index_text_trigrams(
                &search_text,
                doc_id,
                SESSION_TRIGRAMS,
                SESSION_DOC_PATHS,
                SESSION_PATH_IDS,
                &entry.session_id,
            )?;
        }

        // Update cross-reference
        self.update_file_xref(&entry.session_id, &entry.files)?;

        self.persist_doc_ids()?;
        Ok(())
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionEntry>> {
        let txn = self.db.begin_read()?;
        let sessions = txn.open_table(SESSIONS)?;
        match sessions.get(session_id)? {
            Some(data) => Ok(Some(serde_json::from_slice(data.value())?)),
            None => Ok(None),
        }
    }

    pub fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<SessionEntry>> {
        let candidate_ids = self.search_trigrams(query, SESSION_TRIGRAMS, SESSION_DOC_PATHS)?;
        let txn = self.db.begin_read()?;
        let sessions = txn.open_table(SESSIONS)?;

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        for id in &candidate_ids {
            if let Some(data) = sessions.get(id.as_str())? {
                let entry: SessionEntry = serde_json::from_slice(data.value())?;
                // Verify match
                let text = format!(
                    "{} {}",
                    entry.first_message.as_deref().unwrap_or(""),
                    &entry.all_user_text
                );
                if text.to_lowercase().contains(&query_lower) {
                    results.push(entry);
                }
            }
        }

        // Sort by started_at descending
        results.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        results.truncate(limit);
        Ok(results)
    }

    pub fn list_sessions(&self, project: Option<&str>, limit: usize) -> Result<Vec<SessionEntry>> {
        let txn = self.db.begin_read()?;

        if let Some(proj) = project {
            let by_project = txn.open_table(SESSIONS_BY_PROJECT)?;
            let sessions = txn.open_table(SESSIONS)?;
            let prefix = format!("{}\0", proj);
            let mut results = Vec::new();

            // Collect all matching entries, then sort
            for entry in by_project.range(prefix.as_str()..)? {
                let (key, _) = entry?;
                let k = key.value();
                if !k.starts_with(&prefix) {
                    break;
                }
                // Extract session_id from composite key
                let parts: Vec<&str> = k.splitn(3, '\0').collect();
                if parts.len() == 3 {
                    if let Some(data) = sessions.get(parts[2])? {
                        results.push(serde_json::from_slice::<SessionEntry>(data.value())?);
                    }
                }
            }
            results.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            results.truncate(limit);
            Ok(results)
        } else {
            let sessions = txn.open_table(SESSIONS)?;
            let mut results = Vec::new();
            for entry in sessions.range::<&str>(..)? {
                let (_, data) = entry?;
                results.push(serde_json::from_slice::<SessionEntry>(data.value())?);
            }
            results.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            results.truncate(limit);
            Ok(results)
        }
    }

    pub fn session_stats(&self) -> Result<MemoryStats> {
        let txn = self.db.begin_read()?;
        let sessions = txn.open_table(SESSIONS)?;
        let events = txn.open_table(EVENTS)?;
        let agents = txn.open_table(AGENTS)?;

        let mut stats = MemoryStats {
            session_count: sessions.len()?,
            event_count: events.len()?,
            agent_count: agents.len()?,
            ..Default::default()
        };

        let mut tool_counts: HashMap<String, u64> = HashMap::new();
        let mut oldest: Option<String> = None;
        let mut newest: Option<String> = None;

        for entry in sessions.range::<&str>(..)? {
            let (_, data) = entry?;
            let session: SessionEntry = serde_json::from_slice(data.value())?;
            stats.total_turns += session.turn_count as u64;
            stats.total_tool_calls += session.tool_call_count as u64;
            for tool in &session.tool_names {
                *tool_counts.entry(tool.clone()).or_default() += session.tool_call_count as u64;
            }
            if let Some(ref started) = session.started_at {
                if oldest.as_ref().map_or(true, |o| started < o) {
                    oldest = Some(started.clone());
                }
                if newest.as_ref().map_or(true, |n| started > n) {
                    newest = Some(started.clone());
                }
            }
        }

        stats.oldest_session = oldest;
        stats.newest_session = newest;

        let mut tools: Vec<(String, u64)> = tool_counts.into_iter().collect();
        tools.sort_by(|a, b| b.1.cmp(&a.1));
        tools.truncate(10);
        stats.top_tools = tools;

        Ok(stats)
    }

    // ── Event operations ──

    pub fn insert_event(&self, entry: &EventEntry) -> Result<bool> {
        // Dedup check
        let dedup_key = format!(
            "{}\0{}\0{}",
            entry.event_ts,
            entry.session_id,
            &entry.command[..entry.command.len().min(100)]
        );

        let txn = self.db.begin_write()?;
        {
            let dedup = txn.open_table(EVENTS_DEDUP)?;
            if dedup.get(dedup_key.as_str())?.is_some() {
                return Ok(false); // duplicate
            }
        }
        txn.commit()?;

        let event_id = self.alloc_event_id();
        let data = serde_json::to_vec(entry)?;
        let project_key = format!("{}\0{}", entry.project_dir, entry.event_ts);

        let txn = self.db.begin_write()?;
        {
            let mut events = txn.open_table(EVENTS)?;
            let mut by_project = txn.open_table(EVENTS_BY_PROJECT)?;
            let mut dedup = txn.open_table(EVENTS_DEDUP)?;

            events.insert(event_id, data.as_slice())?;
            by_project.insert(project_key.as_str(), event_id)?;
            dedup.insert(dedup_key.as_str(), ())?;
        }
        txn.commit()?;

        // Update trigram index
        let search_text = format!("{} {}", entry.command, entry.project_dir);
        if search_text.len() >= 3 {
            let doc_id = self.alloc_event_doc_id();
            let id_key = format!("evt_{}", event_id);
            self.index_text_trigrams(
                &search_text,
                doc_id,
                EVENT_TRIGRAMS,
                EVENT_DOC_PATHS,
                EVENT_PATH_IDS,
                &id_key,
            )?;
        }

        self.persist_doc_ids()?;
        Ok(true)
    }

    pub fn search_events(&self, query: &str, limit: usize) -> Result<Vec<EventEntry>> {
        let candidate_ids = self.search_trigrams(query, EVENT_TRIGRAMS, EVENT_DOC_PATHS)?;
        let txn = self.db.begin_read()?;
        let events = txn.open_table(EVENTS)?;

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        // candidate_ids are string keys like "evt_123"
        for id_str in &candidate_ids {
            if let Some(num_str) = id_str.strip_prefix("evt_") {
                if let Ok(eid) = num_str.parse::<u64>() {
                    if let Some(data) = events.get(eid)? {
                        let entry: EventEntry = serde_json::from_slice(data.value())?;
                        let text = format!("{} {}", entry.command, entry.project_dir);
                        if text.to_lowercase().contains(&query_lower) {
                            results.push(entry);
                        }
                    }
                }
            }
        }

        results.sort_by(|a, b| b.event_ts.cmp(&a.event_ts));
        results.truncate(limit);
        Ok(results)
    }

    pub fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<EventEntry>> {
        let txn = self.db.begin_read()?;
        let events = txn.open_table(EVENTS)?;

        let mut results = Vec::new();
        if let Some(proj) = project {
            let by_project = txn.open_table(EVENTS_BY_PROJECT)?;
            let prefix = format!("{}\0", proj);
            for entry in by_project.range(prefix.as_str()..)? {
                let (key, val) = entry?;
                if !key.value().starts_with(&prefix) {
                    break;
                }
                let eid = val.value();
                if let Some(data) = events.get(eid)? {
                    results.push(serde_json::from_slice::<EventEntry>(data.value())?);
                }
            }
        } else {
            for entry in events.range::<u64>(..)? {
                let (_, data) = entry?;
                results.push(serde_json::from_slice::<EventEntry>(data.value())?);
            }
        }

        results.sort_by(|a, b| b.event_ts.cmp(&a.event_ts));
        results.truncate(limit);
        Ok(results)
    }

    pub fn get_event_cursor(&self) -> Result<u64> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENT_CURSOR)?;
        Ok(table.get(0u8)?.map(|v| v.value()).unwrap_or(0))
    }

    pub fn set_event_cursor(&self, offset: u64) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(EVENT_CURSOR)?;
            table.insert(0u8, offset)?;
        }
        txn.commit()?;
        Ok(())
    }

    // ── Agent operations ──

    pub fn upsert_agent(&self, entry: &AgentEntry) -> Result<()> {
        let data = serde_json::to_vec(entry)?;
        let parent_key = format!("{}\0{}", entry.parent_session_id, entry.agent_id);

        let txn = self.db.begin_write()?;
        {
            let mut agents = txn.open_table(AGENTS)?;
            let mut by_parent = txn.open_table(AGENTS_BY_PARENT)?;
            agents.insert(entry.agent_id.as_str(), data.as_slice())?;
            by_parent.insert(parent_key.as_str(), b"".as_slice())?;
        }
        txn.commit()?;

        // Update trigram index
        let search_text = format!(
            "{} {}",
            entry.first_message.as_deref().unwrap_or(""),
            &entry.all_user_text
        );
        if search_text.len() >= 3 {
            let doc_id = self.alloc_agent_doc_id();
            self.index_text_trigrams(
                &search_text,
                doc_id,
                AGENT_TRIGRAMS,
                AGENT_DOC_PATHS,
                AGENT_PATH_IDS,
                &entry.agent_id,
            )?;
        }

        self.persist_doc_ids()?;
        Ok(())
    }

    pub fn search_agents(&self, query: &str, parent: Option<&str>, limit: usize) -> Result<Vec<AgentEntry>> {
        let candidate_ids = self.search_trigrams(query, AGENT_TRIGRAMS, AGENT_DOC_PATHS)?;
        let txn = self.db.begin_read()?;
        let agents = txn.open_table(AGENTS)?;

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        for id in &candidate_ids {
            if let Some(data) = agents.get(id.as_str())? {
                let entry: AgentEntry = serde_json::from_slice(data.value())?;
                if let Some(p) = parent {
                    if entry.parent_session_id != p {
                        continue;
                    }
                }
                let text = format!(
                    "{} {}",
                    entry.first_message.as_deref().unwrap_or(""),
                    &entry.all_user_text
                );
                if text.to_lowercase().contains(&query_lower) {
                    results.push(entry);
                }
            }
        }

        results.sort_by(|a, b| b.first_seen_at.cmp(&a.first_seen_at));
        results.truncate(limit);
        Ok(results)
    }

    pub fn list_agents_by_parent(&self, parent_session_id: &str) -> Result<Vec<AgentEntry>> {
        let txn = self.db.begin_read()?;
        let by_parent = txn.open_table(AGENTS_BY_PARENT)?;
        let agents = txn.open_table(AGENTS)?;

        let prefix = format!("{}\0", parent_session_id);
        let mut results = Vec::new();
        for entry in by_parent.range(prefix.as_str()..)? {
            let (key, _) = entry?;
            let k = key.value();
            if !k.starts_with(&prefix) {
                break;
            }
            let parts: Vec<&str> = k.splitn(2, '\0').collect();
            if parts.len() == 2 {
                if let Some(data) = agents.get(parts[1])? {
                    results.push(serde_json::from_slice::<AgentEntry>(data.value())?);
                }
            }
        }
        Ok(results)
    }

    // ── Cross-reference operations ──

    fn update_file_xref(&self, session_id: &str, files: &[String]) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut xref = txn.open_table(SESSION_FILES_XREF)?;
            for file in files {
                let mut session_ids: Vec<String> = match xref.get(file.as_str())? {
                    Some(data) => serde_json::from_slice(data.value()).unwrap_or_default(),
                    None => Vec::new(),
                };
                if !session_ids.contains(&session_id.to_string()) {
                    session_ids.push(session_id.to_string());
                    let data = serde_json::to_vec(&session_ids)?;
                    xref.insert(file.as_str(), data.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn sessions_for_file(&self, file_path: &str, limit: usize) -> Result<Vec<SessionEntry>> {
        let txn = self.db.begin_read()?;
        let xref = txn.open_table(SESSION_FILES_XREF)?;
        let _sessions = txn.open_table(SESSIONS)?;

        // Direct xref lookup
        let mut session_ids: Vec<String> = match xref.get(file_path)? {
            Some(data) => serde_json::from_slice(data.value()).unwrap_or_default(),
            None => Vec::new(),
        };

        // Also try trigram search for the filename
        drop(txn);
        let filename = Path::new(file_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if filename.len() >= 3 {
            if let Ok(extra_ids) = self.search_trigrams(&filename, SESSION_TRIGRAMS, SESSION_DOC_PATHS) {
                for id in extra_ids {
                    if !session_ids.contains(&id) {
                        session_ids.push(id);
                    }
                }
            }
        }

        let txn2 = self.db.begin_read()?;
        let sessions2 = txn2.open_table(SESSIONS)?;
        let mut results = Vec::new();
        for id in &session_ids {
            if let Some(data) = sessions2.get(id.as_str())? {
                results.push(serde_json::from_slice::<SessionEntry>(data.value())?);
            }
        }
        results.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        results.truncate(limit);
        Ok(results)
    }

    pub fn files_for_session(&self, session_id: &str) -> Result<Vec<String>> {
        match self.get_session(session_id)? {
            Some(entry) => Ok(entry.files),
            None => Ok(Vec::new()),
        }
    }
}
