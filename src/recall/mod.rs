//! `ndx recall` — per-project structured episodic memory palace.
//!
//! Implements the subsystem defined in `docs/specs/recall.md`. This module
//! owns the redb schema (spec §5), drawer/room/link CRUD, identity handling,
//! and palace lifecycle. Retrieval (L1/L2/L3), mining, cross-references,
//! and hook integration live in Phase 2+ and will extend this module without
//! changing the schema.
//!
//! The schema is created at version 1 (R-172). All tables are opened up
//! front even when not yet populated (e.g. `drawer_trigrams`,
//! `drawer_embeddings`, cross-ref tables), so later phases do not require
//! schema migration.

pub mod embed;
pub mod error;
pub mod identity;
pub mod mine;
pub mod search;

use anyhow::{Context, Result};
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use embed::{Embedder, EMBEDDING_DIM, MODEL_ID};

pub use error::{ExitCode, RecallError};

// ── Schema constants ──

pub const SCHEMA_VERSION: u32 = 1;
pub const UNCLASSIFIED_ROOM: &str = "unclassified";
pub const DEFAULT_IMPORTANCE: u8 = 5;
pub const MAX_DRAWER_TEXT_BYTES: usize = 8 * 1024;
/// Upper bound on drawers per write transaction during mining (R-631).
pub const MINE_BATCH_SIZE: usize = 1000;

// Table definitions. Values are serde_json bytes unless noted.

/// drawer_id → serialized Drawer
const DRAWERS: TableDefinition<u64, &[u8]> = TableDefinition::new("drawers");
/// 32-byte BLAKE3 content_hash → drawer_id  (R-102 dedup)
const DRAWER_BY_HASH: TableDefinition<&[u8], u64> =
    TableDefinition::new("drawer_by_hash");
/// drawer_id → raw f32 little-endian bytes, 384*4 = 1536 bytes (Phase 3)
const DRAWER_EMBEDDINGS: TableDefinition<u64, &[u8]> =
    TableDefinition::new("drawer_embeddings");
/// room_name → packed u64 little-endian drawer_ids
const DRAWERS_BY_ROOM: TableDefinition<&str, &[u8]> =
    TableDefinition::new("drawers_by_room");
/// room_name → serialized Room
const ROOMS: TableDefinition<&str, &[u8]> = TableDefinition::new("rooms");
/// trigram (3 bytes) → packed u64 drawer_ids (Phase 3)
const DRAWER_TRIGRAMS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("drawer_trigrams");
/// link key = from.to_le_bytes() ++ to.to_le_bytes() ++ [kind_tag] → ()
const LINKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("links");
/// project-relative file path → packed u64 drawer_ids (Phase 4)
const FILE_DRAWER_XREF: TableDefinition<&str, &[u8]> =
    TableDefinition::new("file_drawer_xref");
/// session_id → packed u64 drawer_ids
const SESSION_DRAWER_XREF: TableDefinition<&str, &[u8]> =
    TableDefinition::new("session_drawer_xref");
/// commit_sha → packed u64 drawer_ids (Phase 4)
const COMMIT_DRAWER_XREF: TableDefinition<&str, &[u8]> =
    TableDefinition::new("commit_drawer_xref");
/// claude_session_id → unix-seconds timestamp (Phase 5)
const WAKE_INJECTED: TableDefinition<&str, u64> =
    TableDefinition::new("wake_injected");
/// key → value (schema_version, next_drawer_id, embedding_model, etc.)
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

// ── Data models ──

/// Source of a drawer (R-103).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SourceKind {
    Memory,
    Chroma,
    Project,
    Manual,
    Hook,
}

/// Atomic unit of stored memory (R-100 series).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drawer {
    pub id: u64,
    pub text: String,
    /// BLAKE3 of `text`, 32 bytes. Hex-encoded for JSON readability.
    pub content_hash: String,
    pub room: String,
    pub wing: Option<String>,
    pub importance: u8,
    pub source_kind: SourceKind,
    pub source_session_id: Option<String>,
    pub source_file: Option<String>,
    pub source_line: Option<u32>,
    pub source_commit: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub metadata: BTreeMap<String, String>,
}

/// A named topic bucket (R-110 series).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub created_at: i64,
}

/// Link kind (R-122).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LinkKind {
    References = 0,
    Contradicts = 1,
    Supersedes = 2,
    DerivedFrom = 3,
}

impl LinkKind {
    pub fn from_tag(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::References),
            1 => Some(Self::Contradicts),
            2 => Some(Self::Supersedes),
            3 => Some(Self::DerivedFrom),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "references" | "ref" => Some(Self::References),
            "contradicts" | "contradict" => Some(Self::Contradicts),
            "supersedes" | "supersede" => Some(Self::Supersedes),
            "derivedfrom" | "derived_from" | "derived-from" => Some(Self::DerivedFrom),
            _ => None,
        }
    }
}

/// Outcome of a drawer insert. `deduped = true` means the content hash
/// matched an existing drawer and importance was bumped instead of a new row
/// being written.
#[derive(Debug, Clone, Copy)]
pub struct DrawerInsertOutcome {
    pub id: u64,
    pub deduped: bool,
}

/// Stats for `ndx recall status`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PalaceStats {
    pub drawer_count: u64,
    pub room_count: u64,
    pub link_count: u64,
    pub schema_version: u32,
    pub embedding_model: Option<String>,
    pub last_mined_at: Option<i64>,
    pub created_at: Option<i64>,
}

// ── Palace ──

/// Handle to a per-project recall palace (`.ndx/recall.redb`).
pub struct Palace {
    db: Database,
    project_root: PathBuf,
    next_drawer_id: AtomicU64,
    embedder: OnceLock<Embedder>,
}

impl Palace {
    /// Walk up from CWD looking for an existing `.ndx/recall.redb`. Returns
    /// `None` if not found. For `init`, use [`Self::create_at`] directly.
    pub fn find() -> Result<Option<PathBuf>> {
        let cwd = std::env::current_dir().context("failed to get current directory")?;
        let mut cur: &Path = cwd.as_path();
        loop {
            let candidate = cur.join(".ndx").join("recall.redb");
            if candidate.is_file() {
                return Ok(Some(cur.to_path_buf()));
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => return Ok(None),
            }
        }
    }

    /// Open the palace rooted at the current (or walked-up) project.
    /// Returns `RecallError::not_initialized` if no `recall.redb` exists.
    pub fn open_from_cwd() -> Result<Self> {
        let root = Self::find()?
            .ok_or_else(|| anyhow::Error::from(RecallError::not_initialized()))?;
        Self::open_at(root)
    }

    /// Open an existing palace at a known project root.
    pub fn open_at(project_root: PathBuf) -> Result<Self> {
        let db_path = project_root.join(".ndx").join("recall.redb");
        if !db_path.exists() {
            return Err(RecallError::not_initialized().into());
        }
        Self::open_or_create(project_root, db_path, false)
    }

    /// Create (or reopen) the palace at a specific project root. Used by
    /// `ndx recall init`. `project_root` is assumed to exist.
    pub fn create_at(project_root: PathBuf) -> Result<Self> {
        let ndx_dir = project_root.join(".ndx");
        std::fs::create_dir_all(&ndx_dir).with_context(|| {
            format!("creating {}", ndx_dir.display())
        })?;
        let db_path = ndx_dir.join("recall.redb");
        Self::open_or_create(project_root, db_path, true)
    }

    fn open_or_create(
        project_root: PathBuf,
        db_path: PathBuf,
        init: bool,
    ) -> Result<Self> {
        let db = Database::create(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        // Open all tables in a single write txn so the schema is pinned at
        // version 1 from the start. Tables not yet used in Phase 1 still
        // exist so later phases do not require migrations.
        {
            let txn = db.begin_write()?;
            txn.open_table(DRAWERS)?;
            txn.open_table(DRAWER_BY_HASH)?;
            txn.open_table(DRAWER_EMBEDDINGS)?;
            txn.open_table(DRAWERS_BY_ROOM)?;
            txn.open_table(ROOMS)?;
            txn.open_table(DRAWER_TRIGRAMS)?;
            txn.open_table(LINKS)?;
            txn.open_table(FILE_DRAWER_XREF)?;
            txn.open_table(SESSION_DRAWER_XREF)?;
            txn.open_table(COMMIT_DRAWER_XREF)?;
            txn.open_table(WAKE_INJECTED)?;
            txn.open_table(META)?;
            txn.commit()?;
        }

        // Initialise or validate schema_version.
        {
            let rtxn = db.begin_read()?;
            let meta = rtxn.open_table(META)?;
            match meta.get("schema_version")? {
                Some(v) => {
                    let bytes = v.value();
                    let stored = u32_from_bytes(bytes);
                    if stored > SCHEMA_VERSION {
                        return Err(RecallError::schema_version(format!(
                            "palace schema version {} exceeds supported maximum {}",
                            stored, SCHEMA_VERSION
                        ))
                        .into());
                    }
                }
                None => {
                    drop(meta);
                    drop(rtxn);
                    let wtxn = db.begin_write()?;
                    {
                        let mut meta = wtxn.open_table(META)?;
                        meta.insert(
                            "schema_version",
                            SCHEMA_VERSION.to_le_bytes().as_slice(),
                        )?;
                        meta.insert(
                            "next_drawer_id",
                            0u64.to_le_bytes().as_slice(),
                        )?;
                        meta.insert(
                            "created_at",
                            now_unix().to_le_bytes().as_slice(),
                        )?;
                    }
                    wtxn.commit()?;
                }
            }
        }

        // Load next_drawer_id counter.
        let next_id = {
            let rtxn = db.begin_read()?;
            let meta = rtxn.open_table(META)?;
            meta.get("next_drawer_id")?
                .map(|v| u64_from_bytes(v.value()))
                .unwrap_or(0)
        };

        let palace = Self {
            db,
            project_root,
            next_drawer_id: AtomicU64::new(next_id),
            embedder: OnceLock::new(),
        };

        if init {
            palace.ensure_room(UNCLASSIFIED_ROOM, None, None)?;
        }
        Ok(palace)
    }

    /// Global model cache directory: `~/.ndx/models/`.
    pub fn model_cache_dir() -> Result<PathBuf> {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(home.join(".ndx").join("models"))
    }

    /// Lazily load the embedder (downloads the model on first call) and
    /// return a reference valid for the lifetime of the palace.
    pub fn embedder(&self) -> Result<&Embedder> {
        if let Some(e) = self.embedder.get() {
            return Ok(e);
        }
        let cache = Self::model_cache_dir()?;
        let e = Embedder::load(cache)?;
        // record the model id in META on first load
        {
            let txn = self.db.begin_write()?;
            {
                let mut meta = txn.open_table(META)?;
                meta.insert("embedding_model", MODEL_ID.as_bytes())?;
            }
            txn.commit()?;
        }
        let _ = self.embedder.set(e);
        Ok(self.embedder.get().expect("embedder just set"))
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn db_path(&self) -> PathBuf {
        self.project_root.join(".ndx").join("recall.redb")
    }

    // ── Meta ──

    fn alloc_drawer_id(&self) -> u64 {
        self.next_drawer_id.fetch_add(1, Ordering::Relaxed)
    }

    // ── Rooms (R-110 series) ──

    /// Create a room if missing. Returns true if newly created.
    pub fn ensure_room(
        &self,
        name: &str,
        title: Option<String>,
        description: Option<String>,
    ) -> Result<bool> {
        validate_room_name(name)?;
        let txn = self.db.begin_write()?;
        let created;
        {
            let mut rooms = txn.open_table(ROOMS)?;
            if rooms.get(name)?.is_some() {
                created = false;
            } else {
                let room = Room {
                    name: name.to_string(),
                    title,
                    description,
                    created_at: now_unix(),
                };
                let bytes = serde_json::to_vec(&room)?;
                rooms.insert(name, bytes.as_slice())?;
                created = true;
            }
        }
        txn.commit()?;
        Ok(created)
    }

    pub fn get_room(&self, name: &str) -> Result<Option<Room>> {
        let rtxn = self.db.begin_read()?;
        let rooms = rtxn.open_table(ROOMS)?;
        match rooms.get(name)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_rooms(&self) -> Result<Vec<Room>> {
        let rtxn = self.db.begin_read()?;
        let rooms = rtxn.open_table(ROOMS)?;
        let mut out: Vec<Room> = Vec::new();
        for entry in rooms.iter()? {
            let (_, v) = entry?;
            let room: Room = serde_json::from_slice(v.value())?;
            out.push(room);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Delete a room. Fails with a constraint error (R-114) if it still
    /// contains drawers.
    pub fn delete_room(&self, name: &str) -> Result<()> {
        if name == UNCLASSIFIED_ROOM {
            return Err(RecallError::constraint(
                "the `unclassified` room is reserved and cannot be removed",
            )
            .into());
        }
        let drawer_ids = self.drawer_ids_in_room(name)?;
        if !drawer_ids.is_empty() {
            return Err(RecallError::constraint(format!(
                "room `{}` contains {} drawers; reassign or delete them first",
                name,
                drawer_ids.len()
            ))
            .into());
        }
        let txn = self.db.begin_write()?;
        {
            let mut rooms = txn.open_table(ROOMS)?;
            if rooms.remove(name)?.is_none() {
                return Err(RecallError::constraint(format!("room `{}` not found", name))
                    .into());
            }
            let mut by_room = txn.open_table(DRAWERS_BY_ROOM)?;
            by_room.remove(name)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Rename a room and update all drawer records atomically (R-435).
    pub fn rename_room(&self, old: &str, new: &str) -> Result<u64> {
        validate_room_name(new)?;
        if old == new {
            return Ok(0);
        }
        if old == UNCLASSIFIED_ROOM {
            return Err(RecallError::constraint(
                "the `unclassified` room cannot be renamed",
            )
            .into());
        }
        let txn = self.db.begin_write()?;
        let moved: u64;
        {
            // 1. Verify old exists, new doesn't; fetch old room, migrate the Room record.
            let old_room_bytes: Vec<u8>;
            {
                let rooms = txn.open_table(ROOMS)?;
                if rooms.get(new)?.is_some() {
                    return Err(RecallError::constraint(format!(
                        "room `{}` already exists",
                        new
                    ))
                    .into());
                }
                let fetched = rooms.get(old)?.map(|v| v.value().to_vec());
                old_room_bytes = fetched.ok_or_else(|| {
                    RecallError::constraint(format!("room `{}` not found", old))
                })?;
            }
            let mut room: Room = serde_json::from_slice(&old_room_bytes)?;
            room.name = new.to_string();
            let new_room_bytes = serde_json::to_vec(&room)?;
            {
                let mut rooms = txn.open_table(ROOMS)?;
                rooms.remove(old)?;
                rooms.insert(new, new_room_bytes.as_slice())?;
            }

            // 2. Move room → drawer_ids index entry.
            let ids_bytes: Vec<u8> = {
                let mut by_room = txn.open_table(DRAWERS_BY_ROOM)?;
                let existing = by_room
                    .get(old)?
                    .map(|v| v.value().to_vec())
                    .unwrap_or_default();
                by_room.remove(old)?;
                if !existing.is_empty() {
                    by_room.insert(new, existing.as_slice())?;
                }
                existing
            };
            let ids = decode_u64_list(&ids_bytes);
            moved = ids.len() as u64;

            // 3. Rewrite each drawer's room field.
            let now = now_unix();
            for id in ids {
                let current_bytes: Option<Vec<u8>>;
                {
                    let drawers = txn.open_table(DRAWERS)?;
                    let fetched = drawers.get(id)?.map(|v| v.value().to_vec());
                    current_bytes = fetched;
                }
                if let Some(bytes) = current_bytes {
                    let mut drawer: Drawer = serde_json::from_slice(&bytes)?;
                    drawer.room = new.to_string();
                    drawer.updated_at = now;
                    let new_bytes = serde_json::to_vec(&drawer)?;
                    let mut drawers = txn.open_table(DRAWERS)?;
                    drawers.insert(id, new_bytes.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(moved)
    }

    fn drawer_ids_in_room(&self, room: &str) -> Result<Vec<u64>> {
        let rtxn = self.db.begin_read()?;
        let by_room = rtxn.open_table(DRAWERS_BY_ROOM)?;
        Ok(by_room
            .get(room)?
            .map(|v| decode_u64_list(v.value()))
            .unwrap_or_default())
    }

    // ── Drawers (Phase 1 primitives only; CLI plumbing in Phase 2/6) ──

    /// Insert a drawer with content-hash dedup (R-102). Computes the
    /// embedding synchronously via the lazy embedder. Returns the stored
    /// id and whether this was a dedup hit.
    pub fn insert_drawer(&self, input: Drawer) -> Result<DrawerInsertOutcome> {
        let embedding = self.embedder()?.embed_one(&input.text)?;
        let txn = self.db.begin_write()?;
        let outcome = self.insert_drawer_in_txn(&txn, input, Some(embedding))?;
        txn.commit()?;
        Ok(outcome)
    }

    /// Insert a drawer WITHOUT computing an embedding. For tests and for
    /// the Phase 3 reembed backfill path.
    #[allow(dead_code)]
    pub fn insert_drawer_no_embedding(
        &self,
        input: Drawer,
    ) -> Result<DrawerInsertOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = self.insert_drawer_in_txn(&txn, input, None)?;
        txn.commit()?;
        Ok(outcome)
    }

    /// Insert many drawers, embedding them in groups of
    /// [`embed::EMBED_BATCH_SIZE`] outside the write transaction, then
    /// committing each batch of at most [`MINE_BATCH_SIZE`] drawers
    /// inside a single write transaction (R-631). Partial failures leave
    /// already-committed batches persisted (R-632).
    pub fn insert_drawers_batch<I: IntoIterator<Item = Drawer>>(
        &self,
        drawers: I,
    ) -> Result<Vec<DrawerInsertOutcome>> {
        let drawers: Vec<Drawer> = drawers.into_iter().collect();
        if drawers.is_empty() {
            return Ok(Vec::new());
        }
        // Compute embeddings in groups, then commit in MINE_BATCH_SIZE
        // transactional chunks. We keep the embedding loop outside the
        // write txn so long embedder latency does not hold the db lock.
        let embedder = self.embedder()?;
        let texts: Vec<String> = drawers.iter().map(|d| d.text.clone()).collect();

        let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(embed::EMBED_BATCH_SIZE) {
            let out = embedder.embed(chunk.to_vec())?;
            embeddings.extend(out);
        }
        assert_eq!(embeddings.len(), drawers.len());

        let mut outcomes = Vec::with_capacity(drawers.len());
        let items: Vec<(Drawer, Vec<f32>)> = drawers.into_iter().zip(embeddings).collect();
        for chunk in items.chunks(MINE_BATCH_SIZE) {
            let txn = self.db.begin_write()?;
            for (drawer, emb) in chunk {
                let outcome =
                    self.insert_drawer_in_txn(&txn, drawer.clone(), Some(emb.clone()))?;
                outcomes.push(outcome);
            }
            txn.commit()?;
        }
        Ok(outcomes)
    }

    /// Body of a drawer insert, scoped to an already-open write transaction.
    /// The caller is responsible for commit. When `embedding` is provided
    /// it must be 384-dim; it is persisted to `DRAWER_EMBEDDINGS`. When
    /// `None`, no embedding row is written and semantic search will skip
    /// the drawer until `ndx recall reembed` backfills it.
    fn insert_drawer_in_txn(
        &self,
        txn: &redb::WriteTransaction,
        mut input: Drawer,
        embedding: Option<Vec<f32>>,
    ) -> Result<DrawerInsertOutcome> {
        if let Some(ref e) = embedding {
            if e.len() != EMBEDDING_DIM {
                anyhow::bail!(
                    "embedding dimension mismatch: expected {}, got {}",
                    EMBEDDING_DIM,
                    e.len()
                );
            }
        }
        if input.text.len() > MAX_DRAWER_TEXT_BYTES {
            input
                .text
                .truncate(MAX_DRAWER_TEXT_BYTES.saturating_sub(16));
            input.text.push_str("… [truncated]");
        }
        let hash = blake3::hash(input.text.as_bytes());
        let hash_bytes: [u8; 32] = *hash.as_bytes();
        input.content_hash = hash.to_hex().to_string();
        if input.importance < 1 {
            input.importance = 1;
        }
        if input.importance > 10 {
            input.importance = 10;
        }
        if input.room.is_empty() {
            input.room = UNCLASSIFIED_ROOM.to_string();
        }
        let now = now_unix();
        if input.created_at == 0 {
            input.created_at = now;
        }
        input.updated_at = now;

        // Probe by hash.
        let existing_id: Option<u64>;
        {
            let by_hash = txn.open_table(DRAWER_BY_HASH)?;
            let fetched = by_hash.get(hash_bytes.as_slice())?.map(|v| v.value());
            existing_id = fetched;
        }

        match existing_id {
            Some(eid) => {
                // Dedup: bump importance on existing drawer.
                let existing_bytes: Option<Vec<u8>>;
                {
                    let drawers = txn.open_table(DRAWERS)?;
                    let fetched = drawers.get(eid)?.map(|v| v.value().to_vec());
                    existing_bytes = fetched;
                }
                if let Some(bytes) = existing_bytes {
                    let mut existing: Drawer = serde_json::from_slice(&bytes)?;
                    existing.importance =
                        existing.importance.saturating_add(1).min(10);
                    existing.updated_at = now;
                    let new_bytes = serde_json::to_vec(&existing)?;
                    let mut drawers = txn.open_table(DRAWERS)?;
                    drawers.insert(eid, new_bytes.as_slice())?;
                }
                Ok(DrawerInsertOutcome {
                    id: eid,
                    deduped: true,
                })
            }
            None => {
                // Allocate a fresh id and persist.
                let id = self.alloc_drawer_id();
                input.id = id;

                {
                    let mut by_hash = txn.open_table(DRAWER_BY_HASH)?;
                    by_hash.insert(hash_bytes.as_slice(), id)?;
                }

                // Ensure target room exists.
                let room_exists: bool;
                {
                    let rooms = txn.open_table(ROOMS)?;
                    let found = rooms.get(input.room.as_str())?.is_some();
                    room_exists = found;
                }
                if !room_exists {
                    let room = Room {
                        name: input.room.clone(),
                        title: None,
                        description: None,
                        created_at: now,
                    };
                    let rb = serde_json::to_vec(&room)?;
                    let mut rooms = txn.open_table(ROOMS)?;
                    rooms.insert(input.room.as_str(), rb.as_slice())?;
                }

                // Persist drawer row.
                let drawer_bytes = serde_json::to_vec(&input)?;
                {
                    let mut drawers = txn.open_table(DRAWERS)?;
                    drawers.insert(id, drawer_bytes.as_slice())?;
                }

                // Maintain room and source-xref indexes.
                add_to_room_index(txn, &input.room, id)?;
                if let Some(sid) = input.source_session_id.as_deref() {
                    add_to_string_index(txn, SESSION_DRAWER_XREF, sid, id)?;
                }
                if let Some(fp) = input.source_file.as_deref() {
                    add_to_string_index(txn, FILE_DRAWER_XREF, fp, id)?;
                }

                // Embedding row (R-131, R-133).
                if let Some(emb) = embedding {
                    let bytes = embed::encode_embedding(&emb);
                    let mut tbl = txn.open_table(DRAWER_EMBEDDINGS)?;
                    tbl.insert(id, bytes.as_slice())?;
                }

                // Trigram posting-list updates (R-141..R-143).
                let trigrams = extract_drawer_trigrams(&input.text);
                if !trigrams.is_empty() {
                    let mut tri_tbl = txn.open_table(DRAWER_TRIGRAMS)?;
                    for tri in trigrams {
                        let key = tri.as_slice();
                        let existing: Vec<u8> = {
                            let fetched = tri_tbl.get(key)?.map(|v| v.value().to_vec());
                            fetched.unwrap_or_default()
                        };
                        let mut ids = decode_u64_list(&existing);
                        if !ids.contains(&id) {
                            ids.push(id);
                            ids.sort_unstable();
                        }
                        let new_bytes = encode_u64_list(&ids);
                        tri_tbl.insert(key, new_bytes.as_slice())?;
                    }
                }

                // Persist next_drawer_id counter.
                let next = self.next_drawer_id.load(Ordering::Relaxed);
                let mut meta = txn.open_table(META)?;
                meta.insert(
                    "next_drawer_id",
                    next.to_le_bytes().as_slice(),
                )?;

                Ok(DrawerInsertOutcome { id, deduped: false })
            }
        }
    }

    /// Record `last_mined_at` in META.
    pub fn mark_last_mined(&self) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            meta.insert("last_mined_at", now_unix().to_le_bytes().as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// List drawers filtered by optional room, ordered by id ascending.
    /// Used by Phase 2's read-only `drawer list` command and by later
    /// skill-facing commands in Phase 6.
    pub fn list_drawers(
        &self,
        room: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Drawer>> {
        let rtxn = self.db.begin_read()?;
        let drawers_tbl = rtxn.open_table(DRAWERS)?;
        let mut out = Vec::new();

        if let Some(room_name) = room {
            // Fast path via the room index.
            let by_room = rtxn.open_table(DRAWERS_BY_ROOM)?;
            let ids_bytes = by_room
                .get(room_name)?
                .map(|v| v.value().to_vec())
                .unwrap_or_default();
            let ids = decode_u64_list(&ids_bytes);
            for id in ids.into_iter().skip(offset).take(limit) {
                if let Some(v) = drawers_tbl.get(id)? {
                    let drawer: Drawer = serde_json::from_slice(v.value())?;
                    out.push(drawer);
                }
            }
        } else {
            // Slow path: full scan.
            let mut seen: usize = 0;
            for entry in drawers_tbl.iter()? {
                let (_, v) = entry?;
                if seen < offset {
                    seen += 1;
                    continue;
                }
                if out.len() >= limit {
                    break;
                }
                let drawer: Drawer = serde_json::from_slice(v.value())?;
                out.push(drawer);
                seen += 1;
            }
        }
        Ok(out)
    }

    pub fn get_drawer(&self, id: u64) -> Result<Option<Drawer>> {
        let rtxn = self.db.begin_read()?;
        let drawers = rtxn.open_table(DRAWERS)?;
        match drawers.get(id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    // ── Stats ──

    pub fn stats(&self) -> Result<PalaceStats> {
        let rtxn = self.db.begin_read()?;
        let drawers = rtxn.open_table(DRAWERS)?;
        let rooms = rtxn.open_table(ROOMS)?;
        let links = rtxn.open_table(LINKS)?;
        let meta = rtxn.open_table(META)?;

        let drawer_count = drawers.len()?;
        let room_count = rooms.len()?;
        let link_count = links.len()?;

        let schema_version = meta
            .get("schema_version")?
            .map(|v| u32_from_bytes(v.value()))
            .unwrap_or(0);

        let embedding_model = meta
            .get("embedding_model")?
            .and_then(|v| String::from_utf8(v.value().to_vec()).ok());

        let last_mined_at = meta
            .get("last_mined_at")?
            .map(|v| i64_from_bytes(v.value()));

        let created_at = meta
            .get("created_at")?
            .map(|v| i64_from_bytes(v.value()));

        Ok(PalaceStats {
            drawer_count,
            room_count,
            link_count,
            schema_version,
            embedding_model,
            last_mined_at,
            created_at,
        })
    }

    // ── Search-support readers (used by search.rs and the reembed path) ──

    /// Iterate every (drawer_id, embedding) pair. Drawers without an
    /// embedding row are omitted.
    pub fn iter_embeddings(&self) -> Result<Vec<(u64, Vec<f32>)>> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(DRAWER_EMBEDDINGS)?;
        let mut out = Vec::new();
        for entry in tbl.iter()? {
            let (k, v) = entry?;
            out.push((k.value(), embed::decode_embedding(v.value())));
        }
        Ok(out)
    }

    /// Look up the drawer ids carrying a given trigram.
    pub fn trigram_postings(&self, tri: &[u8; 3]) -> Result<Vec<u64>> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(DRAWER_TRIGRAMS)?;
        let ids: Vec<u8>;
        {
            let fetched = tbl.get(tri.as_slice())?.map(|v| v.value().to_vec());
            ids = fetched.unwrap_or_default();
        }
        Ok(decode_u64_list(&ids))
    }

    /// Look up the set of drawer ids in a given room (Phase 1 helper, now
    /// also used for the search `--room` pre-filter).
    pub fn drawer_ids_in_room_public(&self, room: &str) -> Result<Vec<u64>> {
        self.drawer_ids_in_room(room)
    }

    /// Enumerate every drawer id currently stored (slow: full scan).
    pub fn iter_all_drawer_ids(&self) -> Result<Vec<u64>> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(DRAWERS)?;
        let mut out = Vec::new();
        for entry in tbl.iter()? {
            let (k, _) = entry?;
            out.push(k.value());
        }
        Ok(out)
    }

    /// Create (or update) a link between two drawers. Idempotent: a
    /// repeat call with the same `(from, to, kind)` is a no-op.
    pub fn link_drawers(&self, from: u64, to: u64, kind: LinkKind) -> Result<()> {
        let mut key = [0u8; 17];
        key[..8].copy_from_slice(&from.to_le_bytes());
        key[8..16].copy_from_slice(&to.to_le_bytes());
        key[16] = kind as u8;
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(LINKS)?;
            tbl.insert(key.as_slice(), &[] as &[u8])?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Return true iff at least one drawer has a Supersedes link pointing
    /// at it (R-123 — such drawers are excluded from L1 wake-up).
    pub fn is_superseded(&self, id: u64) -> Result<bool> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(LINKS)?;
        // Scan all link keys for ones ending in (to=id, kind=Supersedes).
        for entry in tbl.iter()? {
            let (k, _) = entry?;
            let key = k.value();
            if key.len() != 17 {
                continue;
            }
            let to = u64::from_le_bytes([
                key[8], key[9], key[10], key[11], key[12], key[13], key[14], key[15],
            ]);
            let kind = key[16];
            if to == id && kind == LinkKind::Supersedes as u8 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Resolve a file path to the drawers that reference it.
    /// Combines two signals (R-901):
    ///   1. `FILE_DRAWER_XREF[file_path]` — drawers whose `source_file`
    ///      matches the given path (project-relative).
    ///   2. Trigram-narrowed candidates whose text contains the file's
    ///      basename as a literal substring.
    ///
    /// Results are deduped and ordered by importance desc, updated_at
    /// desc (R-902).
    pub fn drawers_for_file(&self, file_path: &str) -> Result<Vec<Drawer>> {
        use std::collections::BTreeSet;
        let mut candidates: BTreeSet<u64> = BTreeSet::new();

        // Direct source_file match.
        let direct: Vec<u8>;
        {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(FILE_DRAWER_XREF)?;
            let fetched = tbl.get(file_path)?.map(|v| v.value().to_vec());
            direct = fetched.unwrap_or_default();
        }
        for id in decode_u64_list(&direct) {
            candidates.insert(id);
        }

        // Trigram basename mention.
        let basename = std::path::Path::new(file_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file_path.to_string());
        if !basename.is_empty() {
            let query_tris = extract_query_trigrams(&basename);
            if !query_tris.is_empty() {
                // Intersect: a candidate drawer must carry every query trigram.
                let mut first = true;
                let mut running: std::collections::HashSet<u64> =
                    std::collections::HashSet::new();
                for tri in &query_tris {
                    let posting = self.trigram_postings(tri)?;
                    let set: std::collections::HashSet<u64> =
                        posting.into_iter().collect();
                    if first {
                        running = set;
                        first = false;
                    } else {
                        running.retain(|id| set.contains(id));
                        if running.is_empty() {
                            break;
                        }
                    }
                }
                // Confirm by substring.
                let rtxn = self.db.begin_read()?;
                let drawers_tbl = rtxn.open_table(DRAWERS)?;
                for id in running {
                    if let Some(v) = drawers_tbl.get(id)? {
                        let d: Drawer = serde_json::from_slice(v.value())?;
                        if d.text.contains(&basename) {
                            candidates.insert(id);
                        }
                    }
                }
            }
        }

        // Hydrate + sort.
        let mut out = Vec::with_capacity(candidates.len());
        {
            let rtxn = self.db.begin_read()?;
            let drawers_tbl = rtxn.open_table(DRAWERS)?;
            for id in candidates {
                if let Some(v) = drawers_tbl.get(id)? {
                    out.push(serde_json::from_slice::<Drawer>(v.value())?);
                }
            }
        }
        out.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        Ok(out)
    }

    /// Resolve a session id to the drawers derived from (or mentioning) it.
    /// Backed by `SESSION_DRAWER_XREF` (R-911).
    pub fn drawers_for_session(&self, session_id: &str) -> Result<Vec<Drawer>> {
        let ids_bytes: Vec<u8>;
        {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(SESSION_DRAWER_XREF)?;
            let fetched = tbl.get(session_id)?.map(|v| v.value().to_vec());
            ids_bytes = fetched.unwrap_or_default();
        }
        let ids = decode_u64_list(&ids_bytes);
        let mut out = Vec::with_capacity(ids.len());
        let rtxn = self.db.begin_read()?;
        let drawers_tbl = rtxn.open_table(DRAWERS)?;
        for id in ids {
            if let Some(v) = drawers_tbl.get(id)? {
                out.push(serde_json::from_slice::<Drawer>(v.value())?);
            }
        }
        out.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        Ok(out)
    }

    /// Walk `git diff-tree` for a commit, collect drawers matching any of
    /// the changed files, dedupe, and cache the result in
    /// `COMMIT_DRAWER_XREF[<commit>]` for O(1) repeat lookups (R-921..R-922).
    pub fn drawers_for_commit(&self, commit: &str) -> Result<Vec<Drawer>> {
        // Fast path: cached.
        {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(COMMIT_DRAWER_XREF)?;
            let cached: Option<Vec<u8>> = tbl.get(commit)?.map(|v| v.value().to_vec());
            if let Some(bytes) = cached {
                let ids = decode_u64_list(&bytes);
                let drawers_tbl = rtxn.open_table(DRAWERS)?;
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(v) = drawers_tbl.get(id)? {
                        out.push(serde_json::from_slice::<Drawer>(v.value())?);
                    }
                }
                out.sort_by(|a, b| {
                    b.importance
                        .cmp(&a.importance)
                        .then(b.updated_at.cmp(&a.updated_at))
                });
                return Ok(out);
            }
        }

        // Ask git for the changed files in this commit, run from the
        // project root so relative paths match source_file entries.
        let output = std::process::Command::new("git")
            .current_dir(self.project_root())
            .args(["diff-tree", "--no-commit-id", "--name-only", "-r", commit])
            .output()
            .context("failed to invoke `git`")?;
        if !output.status.success() {
            anyhow::bail!(
                "git diff-tree failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut all_ids: std::collections::BTreeSet<u64> = Default::default();
        for file in &files {
            for d in self.drawers_for_file(file)? {
                all_ids.insert(d.id);
            }
        }

        // Cache the id set.
        let ids_vec: Vec<u64> = all_ids.iter().copied().collect();
        {
            let txn = self.db.begin_write()?;
            {
                let mut tbl = txn.open_table(COMMIT_DRAWER_XREF)?;
                tbl.insert(commit, encode_u64_list(&ids_vec).as_slice())?;
            }
            txn.commit()?;
        }

        // Hydrate + sort.
        let mut out = Vec::with_capacity(ids_vec.len());
        let rtxn = self.db.begin_read()?;
        let drawers_tbl = rtxn.open_table(DRAWERS)?;
        for id in ids_vec {
            if let Some(v) = drawers_tbl.get(id)? {
                out.push(serde_json::from_slice::<Drawer>(v.value())?);
            }
        }
        out.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then(b.updated_at.cmp(&a.updated_at))
        });
        Ok(out)
    }

    /// Reembed all drawers currently missing an embedding row (or all of
    /// them if `force` is true). Used by `ndx recall reembed` and during
    /// development when swapping models.
    pub fn reembed_all(&self, force: bool) -> Result<u64> {
        let embedder = self.embedder()?;

        // Collect ids needing work.
        let ids_to_reembed: Vec<u64> = {
            let rtxn = self.db.begin_read()?;
            let drawers = rtxn.open_table(DRAWERS)?;
            let embeddings = rtxn.open_table(DRAWER_EMBEDDINGS)?;
            let mut ids = Vec::new();
            for entry in drawers.iter()? {
                let (k, _) = entry?;
                let id = k.value();
                if force || embeddings.get(id)?.is_none() {
                    ids.push(id);
                }
            }
            ids
        };

        if ids_to_reembed.is_empty() {
            return Ok(0);
        }

        // Load the drawer texts for those ids.
        let texts: Vec<(u64, String)> = {
            let rtxn = self.db.begin_read()?;
            let drawers = rtxn.open_table(DRAWERS)?;
            let mut out = Vec::with_capacity(ids_to_reembed.len());
            for id in &ids_to_reembed {
                if let Some(v) = drawers.get(*id)? {
                    let d: Drawer = serde_json::from_slice(v.value())?;
                    out.push((*id, d.text));
                }
            }
            out
        };

        let mut count = 0u64;
        for chunk in texts.chunks(embed::EMBED_BATCH_SIZE) {
            let ids_chunk: Vec<u64> = chunk.iter().map(|(id, _)| *id).collect();
            let text_chunk: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
            let vecs = embedder.embed(text_chunk)?;

            let txn = self.db.begin_write()?;
            {
                let mut tbl = txn.open_table(DRAWER_EMBEDDINGS)?;
                for (id, v) in ids_chunk.iter().zip(vecs.iter()) {
                    tbl.insert(*id, embed::encode_embedding(v).as_slice())?;
                }
            }
            txn.commit()?;
            count += chunk.len() as u64;
        }
        Ok(count)
    }
}

// ── Helpers ──

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn u32_from_bytes(b: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    let n = b.len().min(4);
    buf[..n].copy_from_slice(&b[..n]);
    u32::from_le_bytes(buf)
}

fn u64_from_bytes(b: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = b.len().min(8);
    buf[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(buf)
}

fn i64_from_bytes(b: &[u8]) -> i64 {
    u64_from_bytes(b) as i64
}

fn encode_u64_list(ids: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn decode_u64_list(b: &[u8]) -> Vec<u64> {
    b.chunks_exact(8)
        .map(|c| {
            u64::from_le_bytes([
                c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
            ])
        })
        .collect()
}

fn add_to_room_index(txn: &redb::WriteTransaction, room: &str, id: u64) -> Result<()> {
    add_to_string_index(txn, DRAWERS_BY_ROOM, room, id)
}

fn add_to_string_index(
    txn: &redb::WriteTransaction,
    table_def: TableDefinition<&'static str, &'static [u8]>,
    key: &str,
    id: u64,
) -> Result<()> {
    let mut t = txn.open_table(table_def)?;
    let mut ids = t
        .get(key)?
        .map(|v| decode_u64_list(v.value()))
        .unwrap_or_default();
    if !ids.contains(&id) {
        ids.push(id);
        ids.sort_unstable();
        ids.dedup();
    }
    let bytes = encode_u64_list(&ids);
    t.insert(key, bytes.as_slice())?;
    Ok(())
}

/// Extract the set of 3-byte shingles present in a drawer's text.
/// Bytes adjacent to NULs are skipped (treat text containing NUL as
/// non-indexable). This is deliberately simpler than the line-aware
/// `trigram::extract_trigrams_with_lines` used by the file index: drawer
/// search doesn't need per-line granularity.
pub fn extract_drawer_trigrams(text: &str) -> HashSet<[u8; 3]> {
    let bytes = text.as_bytes();
    let mut out: HashSet<[u8; 3]> = HashSet::new();
    for window in bytes.windows(3) {
        if !window.contains(&0) {
            out.insert([window[0], window[1], window[2]]);
        }
    }
    out
}

/// Extract unique query trigrams for L3 lexical lookup.
pub fn extract_query_trigrams(query: &str) -> HashSet<[u8; 3]> {
    extract_drawer_trigrams(query)
}

pub fn validate_room_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(RecallError::usage(format!(
            "invalid room name `{}`: must be 1..=64 ASCII chars",
            name
        ))
        .into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(RecallError::usage(format!(
            "invalid room name `{}`: allowed chars are [a-z0-9_-]",
            name
        ))
        .into());
    }
    Ok(())
}

// ── Project detection helpers (for CLI layer) ──

/// Return the CWD as a canonical project root for `ndx recall init`.
pub fn current_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// Short human-readable name of a project (directory basename).
pub fn project_name(root: &Path) -> String {
    root.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_project() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn create_and_reopen_palace() {
        let dir = tmp_project();
        let root = dir.path().to_path_buf();
        let p = Palace::create_at(root.clone()).unwrap();
        let stats = p.stats().unwrap();
        assert_eq!(stats.schema_version, SCHEMA_VERSION);
        assert_eq!(stats.drawer_count, 0);
        assert_eq!(stats.room_count, 1); // unclassified
        drop(p);
        let p2 = Palace::open_at(root).unwrap();
        let s2 = p2.stats().unwrap();
        assert_eq!(s2.room_count, 1);
    }

    #[test]
    fn ensure_and_list_rooms() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        assert!(p.ensure_room("architecture", None, None).unwrap());
        assert!(!p.ensure_room("architecture", None, None).unwrap());
        let rooms = p.list_rooms().unwrap();
        assert_eq!(rooms.len(), 2);
        assert!(rooms.iter().any(|r| r.name == "architecture"));
    }

    #[test]
    fn delete_nonempty_room_fails() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room("decisions", None, None).unwrap();
        let drawer = Drawer {
            id: 0,
            text: "switched to Postgres because …".to_string(),
            content_hash: String::new(),
            room: "decisions".to_string(),
            wing: None,
            importance: 7,
            source_kind: SourceKind::Manual,
            source_session_id: None,
            source_file: None,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        p.insert_drawer_no_embedding(drawer).unwrap();
        let err = p.delete_room("decisions").unwrap_err();
        let re = err.downcast_ref::<RecallError>().unwrap();
        assert_eq!(re.code, ExitCode::Constraint);
    }

    #[test]
    fn dedup_bumps_importance() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        let mk = || Drawer {
            id: 0,
            text: "same text".to_string(),
            content_hash: String::new(),
            room: UNCLASSIFIED_ROOM.to_string(),
            wing: None,
            importance: 5,
            source_kind: SourceKind::Manual,
            source_session_id: None,
            source_file: None,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        let o1 = p.insert_drawer_no_embedding(mk()).unwrap();
        let o2 = p.insert_drawer_no_embedding(mk()).unwrap();
        assert_eq!(o1.id, o2.id, "dedup should return same id");
        assert!(!o1.deduped);
        assert!(o2.deduped);
        let d = p.get_drawer(o1.id).unwrap().unwrap();
        assert_eq!(d.importance, 6, "importance should bump by 1");
    }

    #[test]
    fn rename_room_updates_drawers() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room("foo", None, None).unwrap();
        let mut d = Drawer {
            id: 0,
            text: "hello".to_string(),
            content_hash: String::new(),
            room: "foo".to_string(),
            wing: None,
            importance: 5,
            source_kind: SourceKind::Manual,
            source_session_id: None,
            source_file: None,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        let o1 = p.insert_drawer_no_embedding(d.clone()).unwrap();
        d.text = "world".into();
        let o2 = p.insert_drawer_no_embedding(d).unwrap();
        assert_ne!(o1.id, o2.id);
        let moved = p.rename_room("foo", "bar").unwrap();
        assert_eq!(moved, 2);
        let d1 = p.get_drawer(o1.id).unwrap().unwrap();
        assert_eq!(d1.room, "bar");
        assert!(p.get_room("foo").unwrap().is_none());
        assert!(p.get_room("bar").unwrap().is_some());
    }

    #[test]
    fn drawers_for_file_dedupes_and_orders() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        // Two drawers whose source_file == "src/auth.rs".
        let mut d = Drawer {
            id: 0,
            text: "line from auth module".into(),
            content_hash: String::new(),
            room: UNCLASSIFIED_ROOM.to_string(),
            wing: None,
            importance: 3,
            source_kind: SourceKind::Project,
            source_session_id: None,
            source_file: Some("src/auth.rs".into()),
            source_line: Some(1),
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        p.insert_drawer_no_embedding(d.clone()).unwrap();
        d.text = "another line from auth module".into();
        d.importance = 9;
        p.insert_drawer_no_embedding(d.clone()).unwrap();
        // A drawer mentioning the basename "auth.rs" from a session.
        let d_text = Drawer {
            id: 0,
            text: "decided to refactor auth.rs heavily".into(),
            content_hash: String::new(),
            room: UNCLASSIFIED_ROOM.to_string(),
            wing: None,
            importance: 7,
            source_kind: SourceKind::Memory,
            source_session_id: Some("abc".into()),
            source_file: None,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        p.insert_drawer_no_embedding(d_text).unwrap();

        let hits = p.drawers_for_file("src/auth.rs").unwrap();
        assert_eq!(hits.len(), 3);
        // Ordered importance desc: 9, 7, 3 (note: dedup bumps importance).
        assert_eq!(hits[0].importance, 9);
        assert_eq!(hits[1].importance, 7);
        assert_eq!(hits[2].importance, 3);
    }

    #[test]
    fn drawers_for_session_uses_xref() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        let d = Drawer {
            id: 0,
            text: "USER: hi\n\nASSISTANT: hello".into(),
            content_hash: String::new(),
            room: UNCLASSIFIED_ROOM.to_string(),
            wing: None,
            importance: 5,
            source_kind: SourceKind::Memory,
            source_session_id: Some("sess-42".into()),
            source_file: None,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        p.insert_drawer_no_embedding(d).unwrap();
        let hits = p.drawers_for_session("sess-42").unwrap();
        assert_eq!(hits.len(), 1);
        let miss = p.drawers_for_session("sess-other").unwrap();
        assert!(miss.is_empty());
    }

    #[test]
    fn validate_room_name_rules() {
        assert!(validate_room_name("architecture").is_ok());
        assert!(validate_room_name("a-b_c_3").is_ok());
        assert!(validate_room_name("Bad").is_err());
        assert!(validate_room_name("").is_err());
        assert!(validate_room_name(&"a".repeat(65)).is_err());
    }
}
