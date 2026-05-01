//! `ndx recall` — per-project structured episodic memory palace.
//!
//! Implements the subsystem defined in `docs/specs/recall.md`. This module
//! owns the redb schema (spec §5), drawer/room/link CRUD, identity handling,
//! and palace lifecycle. Retrieval (L1/L2/L3), mining, cross-references,
//! and hook integration live in Phase 2+ and will extend this module without
//! changing the schema.
//!
//! The schema is created at the current [`SCHEMA_VERSION`]. All tables
//! are opened up front even when not yet populated (e.g. `bm25_postings`,
//! `drawer_embeddings`, cross-ref tables), so steady-state operation
//! does not require per-phase migration steps.

pub mod bm25;
pub mod embed;
pub mod error;
pub mod identity;
pub mod issue;
pub mod mine;
pub mod search;

use anyhow::{Context, Result};
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use embed::{Embedder, EMBEDDING_DIM, MODEL_ID};

pub use error::{ExitCode, RecallError};

// ── Schema constants ──

/// Palace schema version.
///
/// v1 — initial layout (R-172).
/// v2 — drawer-text trigram index replaced by BM25 over a tokenizer.
///      Tables `drawer_trigrams` / `drawers_by_trigram` dropped; new
///      tables `bm25_postings`, `drawers_by_token`, `drawer_lengths`,
///      `bm25_meta`. No auto-migration; palaces predating v2 must be
///      rebuilt via `ndx recall rebuild-index`.
/// v3 — shared palaces (R-1000..R-1072). Adds `canonical_root` META
///      entry; `source_file` is stored canonically-relative. Upgrade
///      via `ndx recall rebuild-index`.
pub const SCHEMA_VERSION: u32 = 3;
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
/// token → packed (u64 drawer_id, u32 tf) posting list for BM25
/// lexical search. Schema v2.
const BM25_POSTINGS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("bm25_postings");
/// drawer_id → packed token list (for cascade on delete). Schema v2.
const DRAWERS_BY_TOKEN: TableDefinition<u64, &[u8]> =
    TableDefinition::new("drawers_by_token");
/// drawer_id → u32 token count (document length for BM25). Schema v2.
const DRAWER_LENGTHS: TableDefinition<u64, u32> =
    TableDefinition::new("drawer_lengths");
/// BM25 corpus stats: "N" → u64, "total_length" → u64. Schema v2.
const BM25_META: TableDefinition<&str, &[u8]> = TableDefinition::new("bm25_meta");
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
/// session_id → source_modified (u64). Tracks sessions already mined
/// so re-runs of `mine --from-memory` skip unchanged sessions.
const MINED_SESSIONS: TableDefinition<&str, u64> =
    TableDefinition::new("mined_sessions");
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

/// Operation that a skill is about to perform on a batch of drawers.
/// Used by `ndx recall drawer list --pending <op> --json` per R-701.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOp {
    Classify,
    Score,
    Dedupe,
    Contradict,
    Summarize,
}

impl PendingOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "classify" => Some(Self::Classify),
            "score" => Some(Self::Score),
            "dedupe" => Some(Self::Dedupe),
            "contradict" => Some(Self::Contradict),
            "summarize" => Some(Self::Summarize),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::Score => "score",
            Self::Dedupe => "dedupe",
            Self::Contradict => "contradict",
            Self::Summarize => "summarize",
        }
    }
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
    /// Canonical project root stamped at init (or on `rebuild-index` for
    /// migrated palaces). `None` on pre-v3 palaces. R-1072.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_root: Option<String>,
    /// Resolved target of the `recall.redb` symlink, if any. R-1072.
    /// Null in JSON when the local palace file is not a symlink.
    #[serde(default)]
    pub palace_linked_to: Option<String>,
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
        Self::open_or_create(project_root, db_path, false, false)
    }

    /// Open a palace without enforcing the schema-version equality check.
    /// Only used by `ndx recall rebuild-index` so a v1 palace can be opened
    /// long enough to rebuild its BM25 tables and bump the version to v2.
    /// Still rejects palaces newer than the binary supports.
    pub fn open_for_migration(project_root: PathBuf) -> Result<Self> {
        let db_path = project_root.join(".ndx").join("recall.redb");
        if !db_path.exists() {
            return Err(RecallError::not_initialized().into());
        }
        Self::open_or_create(project_root, db_path, false, true)
    }

    /// Create (or reopen) the palace at a specific project root. Used by
    /// `ndx recall init`. `project_root` is assumed to exist.
    pub fn create_at(project_root: PathBuf) -> Result<Self> {
        let ndx_dir = project_root.join(".ndx");
        std::fs::create_dir_all(&ndx_dir).with_context(|| {
            format!("creating {}", ndx_dir.display())
        })?;
        let db_path = ndx_dir.join("recall.redb");
        Self::open_or_create(project_root, db_path, true, false)
    }

    fn open_or_create(
        project_root: PathBuf,
        db_path: PathBuf,
        init: bool,
        allow_stale_version: bool,
    ) -> Result<Self> {
        let db = Database::create(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        // Open all tables in a single write txn so the schema is pinned
        // from the start of the process. Tables not currently populated
        // on a fresh palace still exist so steady-state writes do not
        // need to create them lazily.
        {
            let txn = db.begin_write()?;
            txn.open_table(DRAWERS)?;
            txn.open_table(DRAWER_BY_HASH)?;
            txn.open_table(DRAWER_EMBEDDINGS)?;
            txn.open_table(DRAWERS_BY_ROOM)?;
            txn.open_table(ROOMS)?;
            txn.open_table(BM25_POSTINGS)?;
            txn.open_table(DRAWERS_BY_TOKEN)?;
            txn.open_table(DRAWER_LENGTHS)?;
            txn.open_table(BM25_META)?;
            txn.open_table(LINKS)?;
            txn.open_table(FILE_DRAWER_XREF)?;
            txn.open_table(SESSION_DRAWER_XREF)?;
            txn.open_table(COMMIT_DRAWER_XREF)?;
            txn.open_table(WAKE_INJECTED)?;
            txn.open_table(MINED_SESSIONS)?;
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
                    if stored < SCHEMA_VERSION && !allow_stale_version {
                        return Err(RecallError::schema_version(format!(
                            "palace schema version {} is older than supported {}. \
                             Run `ndx recall rebuild-index` to upgrade.",
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
                        // Schema v3: stamp canonical_root at init time.
                        // `project_root` is the absolute path at which the
                        // palace was created; linked secondaries never hit
                        // this branch because they symlink an existing
                        // database whose META already carries the field.
                        let abs = absolute_path(&project_root);
                        if let Some(s) = abs.to_str() {
                            meta.insert("canonical_root", s.as_bytes())?;
                        }
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

    /// Absolute path to the canonical project root stored in META, if set.
    /// Returns `None` on a pre-v3 palace that hasn't been rebuilt yet.
    pub fn canonical_root(&self) -> Result<Option<PathBuf>> {
        let rtxn = self.db.begin_read()?;
        let meta = rtxn.open_table(META)?;
        Ok(meta
            .get("canonical_root")?
            .and_then(|v| String::from_utf8(v.value().to_vec()).ok())
            .map(PathBuf::from))
    }

    /// Rewrite the canonical_root META entry. Used by `ndx recall rehome`.
    /// Does not move the palace file and does not re-normalize drawer
    /// `source_file` entries (spec R-1034).
    pub fn set_canonical_root(&self, new_root: &Path) -> Result<()> {
        let abs = absolute_path(new_root);
        let s = abs
            .to_str()
            .context("canonical_root must be valid UTF-8")?
            .to_string();
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            meta.insert("canonical_root", s.as_bytes())?;
        }
        txn.commit()?;
        Ok(())
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

    /// Insert a drawer WITHOUT computing an embedding. Used by mine
    /// (default fast path) and tests.
    pub fn insert_drawer_no_embedding(
        &self,
        input: Drawer,
    ) -> Result<DrawerInsertOutcome> {
        let txn = self.db.begin_write()?;
        let outcome = self.insert_drawer_in_txn(&txn, input, None)?;
        txn.commit()?;
        Ok(outcome)
    }

    /// Insert many drawers WITHOUT embeddings, batching into transactions
    /// of at most [`MINE_BATCH_SIZE`] per commit. BM25 posting updates are
    /// aggregated per-batch and flushed once per unique token instead of
    /// per-drawer, reducing I/O by ~300x for large batches.
    pub fn insert_drawers_batch_no_embed(
        &self,
        drawers: Vec<Drawer>,
    ) -> Result<Vec<DrawerInsertOutcome>> {
        let mut outcomes = Vec::with_capacity(drawers.len());
        for chunk in drawers.chunks(MINE_BATCH_SIZE) {
            let txn = self.db.begin_write()?;
            // Phase 1: insert rows, hash bindings, room + xref indexes.
            // Collect (new_id, tokens) for batch BM25 flush.
            let mut bm25_batch: Vec<(u64, Vec<String>)> = Vec::new();
            for d in chunk {
                let outcome =
                    self.insert_drawer_in_txn_skip_bm25(&txn, d.clone(), None)?;
                if !outcome.deduped {
                    let tokens = bm25::tokenize(&d.text);
                    if !tokens.is_empty() {
                        bm25_batch.push((outcome.id, tokens));
                    }
                }
                outcomes.push(outcome);
            }
            // Phase 2: flush BM25 state in one sweep.
            if !bm25_batch.is_empty() {
                // Per-drawer: write length and reverse index.
                let mut total_len_delta: u64 = 0;
                let mut doc_count_delta: u64 = 0;
                {
                    let mut lengths = txn.open_table(DRAWER_LENGTHS)?;
                    let mut rev = txn.open_table(DRAWERS_BY_TOKEN)?;
                    for (id, tokens) in &bm25_batch {
                        lengths.insert(*id, tokens.len() as u32)?;
                        rev.insert(*id, encode_token_list(tokens).as_slice())?;
                        total_len_delta += tokens.len() as u64;
                        doc_count_delta += 1;
                    }
                }
                // Aggregate token → list of (id, tf) across the batch.
                let mut agg: std::collections::HashMap<String, Vec<(u64, u32)>> =
                    Default::default();
                for (id, tokens) in bm25_batch {
                    let tf = bm25::term_frequencies(&tokens);
                    for (tok, count) in tf {
                        agg.entry(tok).or_default().push((id, count));
                    }
                }
                let mut posts = txn.open_table(BM25_POSTINGS)?;
                for (tok, new_entries) in agg {
                    let existing: Vec<u8> = posts
                        .get(tok.as_str())?
                        .map(|v| v.value().to_vec())
                        .unwrap_or_default();
                    let mut entries = decode_posting_list(&existing);
                    entries.extend(new_entries);
                    entries.sort_unstable_by_key(|(id, _)| *id);
                    posts.insert(tok.as_str(), encode_posting_list(&entries).as_slice())?;
                }
                // Bump corpus meta.
                let mut meta = txn.open_table(BM25_META)?;
                let prev_n: u64 = meta
                    .get("N")?
                    .map(|v| u64_from_bytes(v.value()))
                    .unwrap_or(0);
                let prev_total: u64 = meta
                    .get("total_length")?
                    .map(|v| u64_from_bytes(v.value()))
                    .unwrap_or(0);
                meta.insert(
                    "N",
                    (prev_n + doc_count_delta).to_le_bytes().as_slice(),
                )?;
                meta.insert(
                    "total_length",
                    (prev_total + total_len_delta).to_le_bytes().as_slice(),
                )?;
            }
            txn.commit()?;
        }
        Ok(outcomes)
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

    /// Like `insert_drawer_in_txn` but skips BM25 posting-list and
    /// corpus-meta writes. The caller aggregates tokens across the batch
    /// and flushes once.
    fn insert_drawer_in_txn_skip_bm25(
        &self,
        txn: &redb::WriteTransaction,
        input: Drawer,
        embedding: Option<Vec<f32>>,
    ) -> Result<DrawerInsertOutcome> {
        self.insert_drawer_in_txn_inner(txn, input, embedding, false)
    }

    /// Body of a drawer insert, scoped to an already-open write transaction.
    /// The caller is responsible for commit. When `embedding` is provided
    /// it must be 384-dim; it is persisted to `DRAWER_EMBEDDINGS`. When
    /// `None`, no embedding row is written and semantic search will skip
    /// the drawer until `ndx recall reembed` backfills it.
    fn insert_drawer_in_txn(
        &self,
        txn: &redb::WriteTransaction,
        input: Drawer,
        embedding: Option<Vec<f32>>,
    ) -> Result<DrawerInsertOutcome> {
        self.insert_drawer_in_txn_inner(txn, input, embedding, true)
    }

    fn insert_drawer_in_txn_inner(
        &self,
        txn: &redb::WriteTransaction,
        mut input: Drawer,
        embedding: Option<Vec<f32>>,
        write_bm25: bool,
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
            safe_truncate(&mut input.text, MAX_DRAWER_TEXT_BYTES.saturating_sub(16));
            input.text.push_str("… [truncated]");
        }
        // R-1021: normalize `source_file` against canonical_root before
        // storage so every write path shares the same representation.
        if let Some(ref sf) = input.source_file {
            let canonical = read_canonical_root(txn)?;
            if let Some(root) = canonical {
                let normalized =
                    normalize_source_file(&root, Path::new(sf));
                input.source_file = Some(normalized.to_string_lossy().into_owned());
            }
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

                // BM25 lexical index updates (R-141..R-143, schema v2).
                // Skipped when `write_bm25 == false` (batch callers
                // aggregate and flush once per unique token instead).
                if write_bm25 {
                    add_drawer_to_bm25(txn, id, &input.text)?;
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

        let canonical_root = meta
            .get("canonical_root")?
            .and_then(|v| String::from_utf8(v.value().to_vec()).ok());

        let palace_linked_to = symlink_resolved_target(&self.db_path());

        Ok(PalaceStats {
            drawer_count,
            room_count,
            link_count,
            schema_version,
            embedding_model,
            last_mined_at,
            created_at,
            canonical_root,
            palace_linked_to,
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

    /// Look up the BM25 posting list for a token: every drawer that
    /// contains the token along with its term frequency.
    pub fn bm25_postings(&self, token: &str) -> Result<Vec<(u64, u32)>> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(BM25_POSTINGS)?;
        let bytes: Vec<u8> = tbl
            .get(token)?
            .map(|v| v.value().to_vec())
            .unwrap_or_default();
        Ok(decode_posting_list(&bytes))
    }

    /// Return (N, total_length, avg_dl) for BM25 scoring.
    /// `avg_dl = 0` iff `N = 0` (empty corpus).
    pub fn bm25_corpus_stats(&self) -> Result<(u64, u64, f32)> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(BM25_META)?;
        let n: u64 = tbl
            .get("N")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        let total: u64 = tbl
            .get("total_length")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        let avg = if n == 0 { 0.0 } else { total as f32 / n as f32 };
        Ok((n, total, avg))
    }

    /// Read a drawer's cached token length for BM25 scoring.
    pub fn drawer_token_length(&self, id: u64) -> Result<u32> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(DRAWER_LENGTHS)?;
        Ok(tbl.get(id)?.map(|v| v.value()).unwrap_or(0))
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

    // ── Drawer mutation (Phase 6 / R-424, R-425, R-427) ──

    /// Update a drawer's mutable fields. Any subset of (room, importance,
    /// text) may be changed. Returns the post-update drawer.
    ///
    /// Changing `text` recomputes the content hash, re-registers the
    /// drawer under its new hash (subject to collision dedup), and
    /// rebuilds its BM25 postings. Importance/room edits do not touch
    /// the embedding or BM25 index.
    pub fn update_drawer(
        &self,
        id: u64,
        new_room: Option<&str>,
        new_importance: Option<u8>,
        new_text: Option<&str>,
    ) -> Result<Drawer> {
        if let Some(r) = new_room {
            validate_room_name(r)?;
        }
        if let Some(i) = new_importance {
            if !(1..=10).contains(&i) {
                return Err(RecallError::usage(
                    "importance must be in 1..=10",
                )
                .into());
            }
        }

        // Fetch current row.
        let current_bytes: Vec<u8> = {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(DRAWERS)?;
            let fetched = tbl.get(id)?.map(|v| v.value().to_vec());
            fetched.ok_or_else(|| {
                RecallError::constraint(format!("drawer {} not found", id))
            })?
        };
        let mut drawer: Drawer = serde_json::from_slice(&current_bytes)?;
        let old_room = drawer.room.clone();
        let old_hash_hex = drawer.content_hash.clone();

        if let Some(r) = new_room {
            drawer.room = r.to_string();
        }
        if let Some(i) = new_importance {
            drawer.importance = i;
        }
        if let Some(t) = new_text {
            drawer.text = t.to_string();
            if drawer.text.len() > MAX_DRAWER_TEXT_BYTES {
                safe_truncate(&mut drawer.text, MAX_DRAWER_TEXT_BYTES.saturating_sub(16));
                drawer.text.push_str("… [truncated]");
            }
            let h = blake3::hash(drawer.text.as_bytes());
            drawer.content_hash = h.to_hex().to_string();
        }
        drawer.updated_at = now_unix();

        let txn = self.db.begin_write()?;
        {
            // Room index maintenance.
            if drawer.room != old_room {
                remove_from_room_index(&txn, &old_room, id)?;
                // Ensure the target room exists so list_rooms shows it
                // after reassignment.
                let needs_create: bool;
                {
                    let rooms = txn.open_table(ROOMS)?;
                    needs_create = rooms.get(drawer.room.as_str())?.is_none();
                }
                if needs_create {
                    let room = Room {
                        name: drawer.room.clone(),
                        title: None,
                        description: None,
                        created_at: now_unix(),
                    };
                    let rb = serde_json::to_vec(&room)?;
                    let mut rooms = txn.open_table(ROOMS)?;
                    rooms.insert(drawer.room.as_str(), rb.as_slice())?;
                }
                add_to_room_index(&txn, &drawer.room, id)?;
            }

            // Hash re-registration and BM25 index rebuild if text changed.
            if new_text.is_some() {
                // Remove old hash → id binding.
                let old_hash_bytes = hex_decode_32(&old_hash_hex)
                    .context("stored content_hash is not valid hex")?;
                {
                    let mut by_hash = txn.open_table(DRAWER_BY_HASH)?;
                    by_hash.remove(old_hash_bytes.as_slice())?;
                }
                // Drop old BM25 contribution (postings, length, corpus meta).
                remove_drawer_from_bm25(&txn, id)?;

                // Insert new hash → id binding (dedup guard).
                let new_hash_bytes = hex_decode_32(&drawer.content_hash)
                    .context("new content_hash is not valid hex")?;
                let existing_for_hash: Option<u64>;
                {
                    let by_hash = txn.open_table(DRAWER_BY_HASH)?;
                    let fetched = by_hash
                        .get(new_hash_bytes.as_slice())?
                        .map(|v| v.value());
                    existing_for_hash = fetched;
                }
                match existing_for_hash {
                    Some(other) if other != id => {
                        // The new text collides with another drawer's content —
                        // refuse. Callers should use drawer rm + drawer add in
                        // that case.
                        return Err(RecallError::constraint(format!(
                            "new text collides with existing drawer {}",
                            other
                        ))
                        .into());
                    }
                    _ => {
                        let mut by_hash = txn.open_table(DRAWER_BY_HASH)?;
                        by_hash.insert(new_hash_bytes.as_slice(), id)?;
                    }
                }

                // Rebuild BM25 state from new text.
                add_drawer_to_bm25(&txn, id, &drawer.text)?;
            }

            // Persist updated drawer row.
            let bytes = serde_json::to_vec(&drawer)?;
            let mut drawers = txn.open_table(DRAWERS)?;
            drawers.insert(id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(drawer)
    }

    /// Apply a patch to a drawer's `metadata` map. Each `(key, value)`
    /// pair either sets the value (`Some`) or removes the key (`None`).
    /// Updates `updated_at`. No indexes need touching — `metadata` is
    /// a plain payload field. Returns the post-patch drawer.
    pub fn patch_drawer_metadata(
        &self,
        id: u64,
        patch: &[(String, Option<String>)],
    ) -> Result<Drawer> {
        let current_bytes: Vec<u8> = {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(DRAWERS)?;
            let fetched = tbl.get(id)?.map(|v| v.value().to_vec());
            fetched.ok_or_else(|| {
                RecallError::constraint(format!("drawer {} not found", id))
            })?
        };
        let mut drawer: Drawer = serde_json::from_slice(&current_bytes)?;
        for (key, value) in patch {
            match value {
                Some(v) => {
                    drawer.metadata.insert(key.clone(), v.clone());
                }
                None => {
                    drawer.metadata.remove(key);
                }
            }
        }
        drawer.updated_at = now_unix();
        let txn = self.db.begin_write()?;
        {
            let bytes = serde_json::to_vec(&drawer)?;
            let mut drawers = txn.open_table(DRAWERS)?;
            drawers.insert(id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(drawer)
    }

    /// Append text to a drawer's existing body. Used by `issue close`
    /// to attach a structured fix-and-commit trailer without losing
    /// the original issue body. Updates BM25, content_hash, etc. via
    /// the existing `update_drawer` plumbing.
    pub fn append_drawer_text(&self, id: u64, suffix: &str) -> Result<Drawer> {
        let current_bytes: Vec<u8> = {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(DRAWERS)?;
            let fetched = tbl.get(id)?.map(|v| v.value().to_vec());
            fetched.ok_or_else(|| {
                RecallError::constraint(format!("drawer {} not found", id))
            })?
        };
        let drawer: Drawer = serde_json::from_slice(&current_bytes)?;
        let combined = format!("{}{}", drawer.text, suffix);
        self.update_drawer(id, None, None, Some(&combined))
    }

    /// Delete a drawer and cascade across every satellite table:
    /// DRAWER_BY_HASH, DRAWER_EMBEDDINGS, DRAWERS_BY_ROOM, BM25_POSTINGS,
    /// DRAWERS_BY_TOKEN, DRAWER_LENGTHS, BM25_META, FILE_DRAWER_XREF,
    /// SESSION_DRAWER_XREF, COMMIT_DRAWER_XREF (best-effort scan), and
    /// LINKS in both directions (R-124).
    pub fn delete_drawer(&self, id: u64) -> Result<bool> {
        // Fetch full row first so we know what indexes to clean.
        let bytes: Option<Vec<u8>> = {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(DRAWERS)?;
            let fetched = tbl.get(id)?.map(|v| v.value().to_vec());
            fetched
        };
        let drawer: Drawer = match bytes {
            Some(b) => serde_json::from_slice(&b)?,
            None => return Ok(false),
        };

        let txn = self.db.begin_write()?;
        {
            // Primary row.
            {
                let mut drawers = txn.open_table(DRAWERS)?;
                drawers.remove(id)?;
            }
            // Content-hash binding.
            if let Ok(h) = hex_decode_32(&drawer.content_hash) {
                let mut by_hash = txn.open_table(DRAWER_BY_HASH)?;
                by_hash.remove(h.as_slice())?;
            }
            // Embedding row.
            {
                let mut tbl = txn.open_table(DRAWER_EMBEDDINGS)?;
                tbl.remove(id)?;
            }
            // Room index.
            remove_from_room_index(&txn, &drawer.room, id)?;
            // BM25 lexical index.
            remove_drawer_from_bm25(&txn, id)?;
            // File xref.
            if let Some(fp) = drawer.source_file.as_deref() {
                remove_from_string_index(&txn, FILE_DRAWER_XREF, fp, id)?;
            }
            // Session xref.
            if let Some(sid) = drawer.source_session_id.as_deref() {
                remove_from_string_index(&txn, SESSION_DRAWER_XREF, sid, id)?;
            }
            // Commit xref is populated on-demand and may contain this id;
            // scan and prune rather than recompute.
            {
                let keys: Vec<(String, Vec<u8>)> = {
                    let tbl = txn.open_table(COMMIT_DRAWER_XREF)?;
                    let mut out = Vec::new();
                    for entry in tbl.iter()? {
                        let (k, v) = entry?;
                        out.push((k.value().to_string(), v.value().to_vec()));
                    }
                    out
                };
                let mut tbl = txn.open_table(COMMIT_DRAWER_XREF)?;
                for (commit, ids_bytes) in keys {
                    let mut ids = decode_u64_list(&ids_bytes);
                    let before = ids.len();
                    ids.retain(|x| *x != id);
                    if ids.len() != before {
                        if ids.is_empty() {
                            tbl.remove(commit.as_str())?;
                        } else {
                            tbl.insert(commit.as_str(), encode_u64_list(&ids).as_slice())?;
                        }
                    }
                }
            }
            // Links in both directions.
            {
                let keys_to_drop: Vec<Vec<u8>> = {
                    let tbl = txn.open_table(LINKS)?;
                    let mut out = Vec::new();
                    for entry in tbl.iter()? {
                        let (k, _) = entry?;
                        let key = k.value();
                        if key.len() != 17 {
                            continue;
                        }
                        let from = u64::from_le_bytes([
                            key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
                        ]);
                        let to = u64::from_le_bytes([
                            key[8], key[9], key[10], key[11], key[12], key[13], key[14],
                            key[15],
                        ]);
                        if from == id || to == id {
                            out.push(key.to_vec());
                        }
                    }
                    out
                };
                let mut tbl = txn.open_table(LINKS)?;
                for k in keys_to_drop {
                    tbl.remove(k.as_slice())?;
                }
            }
        }
        txn.commit()?;
        Ok(true)
    }

    /// Delete matching links. If `kind` is `None`, all links between the
    /// pair are deleted regardless of kind. Returns the number removed.
    pub fn unlink_drawers(
        &self,
        from: u64,
        to: u64,
        kind: Option<LinkKind>,
    ) -> Result<u64> {
        let txn = self.db.begin_write()?;
        let removed: u64;
        {
            let keys_to_drop: Vec<Vec<u8>> = {
                let tbl = txn.open_table(LINKS)?;
                let mut out = Vec::new();
                for entry in tbl.iter()? {
                    let (k, _) = entry?;
                    let key = k.value();
                    if key.len() != 17 {
                        continue;
                    }
                    let f = u64::from_le_bytes([
                        key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
                    ]);
                    let t = u64::from_le_bytes([
                        key[8], key[9], key[10], key[11], key[12], key[13], key[14],
                        key[15],
                    ]);
                    if f != from || t != to {
                        continue;
                    }
                    if let Some(want) = kind {
                        if key[16] != want as u8 {
                            continue;
                        }
                    }
                    out.push(key.to_vec());
                }
                out
            };
            removed = keys_to_drop.len() as u64;
            let mut tbl = txn.open_table(LINKS)?;
            for k in keys_to_drop {
                tbl.remove(k.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    // ── Pending-op discovery (Phase 6 / R-701 + skill contracts) ──

    /// Enumerate drawers needing a given skill operation.
    /// `classify` → `room == "unclassified"`
    /// `score` → `importance == 5 AND source_kind != Manual`
    /// `dedupe`, `contradict`, `summarize` — see `PendingOp` docs.
    pub fn list_pending(&self, op: PendingOp, limit: usize) -> Result<Vec<Drawer>> {
        let all = self.list_drawers(None, usize::MAX, 0)?;
        let filtered: Vec<Drawer> = match op {
            PendingOp::Classify => all
                .into_iter()
                .filter(|d| d.room == UNCLASSIFIED_ROOM)
                .take(limit)
                .collect(),
            PendingOp::Score => all
                .into_iter()
                .filter(|d| d.importance == DEFAULT_IMPORTANCE
                    && !matches!(d.source_kind, SourceKind::Manual))
                .take(limit)
                .collect(),
            PendingOp::Dedupe => {
                // Candidate pool: drawers whose content_hash prefix collides
                // with any other drawer (Phase 6 heuristic v1 — simpler than
                // trigram overlap pair discovery, still useful).
                let mut by_prefix: std::collections::HashMap<String, Vec<Drawer>> =
                    Default::default();
                for d in all {
                    let key = d.content_hash.chars().take(6).collect::<String>();
                    by_prefix.entry(key).or_default().push(d);
                }
                let mut candidates: Vec<Drawer> = Vec::new();
                for (_, group) in by_prefix {
                    if group.len() > 1 {
                        candidates.extend(group);
                    }
                }
                candidates.into_iter().take(limit).collect()
            }
            PendingOp::Contradict => {
                // Placeholder heuristic: drawers that already have an
                // incoming link of any kind are in scope for contradict
                // review. Real contradict-candidate discovery (K-trigram
                // overlap pairs) is a v2 refinement.
                let mut with_incoming: std::collections::HashSet<u64> = Default::default();
                {
                    let rtxn = self.db.begin_read()?;
                    let tbl = rtxn.open_table(LINKS)?;
                    for entry in tbl.iter()? {
                        let (k, _) = entry?;
                        let key = k.value();
                        if key.len() != 17 {
                            continue;
                        }
                        let to = u64::from_le_bytes([
                            key[8], key[9], key[10], key[11], key[12], key[13], key[14],
                            key[15],
                        ]);
                        with_incoming.insert(to);
                    }
                }
                all.into_iter()
                    .filter(|d| with_incoming.contains(&d.id))
                    .take(limit)
                    .collect()
            }
            PendingOp::Summarize => {
                // One representative drawer per non-empty room — the top
                // importance entry per room.
                let mut by_room: BTreeMap<String, Drawer> = BTreeMap::new();
                for d in all {
                    by_room
                        .entry(d.room.clone())
                        .and_modify(|e| {
                            if d.importance > e.importance {
                                *e = d.clone();
                            }
                        })
                        .or_insert(d);
                }
                by_room.into_values().take(limit).collect()
            }
        };
        Ok(filtered)
    }

    // ── Mined-session tracking ──

    /// Return true if a session has already been mined with the same
    /// source_modified timestamp (i.e., the JSONL file hasn't changed).
    pub fn session_already_mined(&self, session_id: &str, source_modified: u64) -> Result<bool> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(MINED_SESSIONS)?;
        match tbl.get(session_id)? {
            Some(v) => Ok(v.value() == source_modified),
            None => Ok(false),
        }
    }

    /// Record that a session has been mined at its current source_modified.
    pub fn mark_session_mined(&self, session_id: &str, source_modified: u64) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(MINED_SESSIONS)?;
            tbl.insert(session_id, source_modified)?;
        }
        txn.commit()?;
        Ok(())
    }

    // ── Bulk operations ──

    /// Update room (and optionally importance) for all drawers matching
    /// a `source_file` prefix. Returns the count of drawers updated.
    /// Used by the classify skill for batch-by-file workflows.
    pub fn bulk_update_by_source_file(
        &self,
        source_file: &str,
        new_room: &str,
        new_importance: Option<u8>,
    ) -> Result<u64> {
        validate_room_name(new_room)?;
        if let Some(i) = new_importance {
            if !(1..=10).contains(&i) {
                return Err(RecallError::usage("importance must be in 1..=10").into());
            }
        }

        // Collect matching ids. We match both exact and prefix so that
        // `--source-file docs/` covers all files under docs/.
        //
        // R-1023/R-1024: callers may pass absolute or cwd-relative paths;
        // normalize to canonical-relative form before matching so shared
        // palaces (where stored paths are always canonical-relative) hit.
        let canonical = self.canonical_root()?;
        let resolved = resolve_query_path(canonical.as_deref(), source_file);
        let resolved_str = resolved.to_string_lossy().into_owned();
        let all = self.list_drawers(None, usize::MAX, 0)?;
        let matching_ids: Vec<u64> = all
            .iter()
            .filter(|d| {
                d.source_file
                    .as_deref()
                    .map(|f| {
                        f == source_file
                            || f.starts_with(source_file)
                            || f == resolved_str.as_str()
                            || f.starts_with(resolved_str.as_str())
                    })
                    .unwrap_or(false)
            })
            .map(|d| d.id)
            .collect();

        if matching_ids.is_empty() {
            return Ok(0);
        }

        let mut count = 0u64;
        for id in matching_ids {
            self.update_drawer(id, Some(new_room), new_importance, None)?;
            count += 1;
        }
        Ok(count)
    }

    /// Update room (and optionally importance) for all drawers whose text
    /// matches `pattern` (case-insensitive regex). Optionally restrict to
    /// drawers currently in `from_room`. Returns matched drawer ids + count.
    pub fn bulk_update_by_search(
        &self,
        pattern: &str,
        new_room: &str,
        new_importance: Option<u8>,
        from_room: Option<&str>,
        dry_run: bool,
    ) -> Result<(Vec<Drawer>, u64)> {
        validate_room_name(new_room)?;
        if let Some(i) = new_importance {
            if !(1..=10).contains(&i) {
                return Err(RecallError::usage("importance must be in 1..=10").into());
            }
        }

        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| RecallError::usage(format!("invalid regex `{}`: {}", pattern, e)))?;

        let all = self.list_drawers(from_room, usize::MAX, 0)?;
        let matched: Vec<Drawer> = all
            .into_iter()
            .filter(|d| re.is_match(&d.text))
            .collect();

        if dry_run || matched.is_empty() {
            let count = matched.len() as u64;
            return Ok((matched, count));
        }

        let mut count = 0u64;
        for d in &matched {
            self.update_drawer(d.id, Some(new_room), new_importance, None)?;
            count += 1;
        }
        Ok((matched, count))
    }

    // ── Wake-up injection state (Phase 5 / R-160 series) ──

    /// Return true if wake-up text has already been injected into the
    /// given Claude session.
    pub fn wake_injection_seen(&self, session_id: &str) -> Result<bool> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(WAKE_INJECTED)?;
        Ok(tbl.get(session_id)?.is_some())
    }

    /// Mark the given session as having received wake-up injection.
    /// Idempotent — repeat calls simply refresh the timestamp.
    pub fn mark_wake_injected(&self, session_id: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(WAKE_INJECTED)?;
            tbl.insert(session_id, now_unix() as u64)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Clear the wake-up injection marker for a specific session.
    pub fn clear_wake_injection(&self, session_id: &str) -> Result<bool> {
        let txn = self.db.begin_write()?;
        let existed: bool;
        {
            let mut tbl = txn.open_table(WAKE_INJECTED)?;
            existed = tbl.remove(session_id)?.is_some();
        }
        txn.commit()?;
        Ok(existed)
    }

    /// Clear all wake-up injection markers. Used by `wake --force` when
    /// the caller has no session id in scope (e.g., user running the
    /// command at a plain shell to pick up identity.toml edits).
    pub fn clear_all_wake_injections(&self) -> Result<u64> {
        let txn = self.db.begin_write()?;
        let cleared: u64;
        {
            let mut tbl = txn.open_table(WAKE_INJECTED)?;
            let keys: Vec<String> = {
                let iter = tbl.iter()?;
                let mut out = Vec::new();
                for entry in iter {
                    let (k, _) = entry?;
                    out.push(k.value().to_string());
                }
                out
            };
            cleared = keys.len() as u64;
            for k in keys {
                tbl.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        Ok(cleared)
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

    /// Return every link originating from `from` as `(to, kind)` pairs.
    /// Mostly used by the issue tracker to verify `derived_from`
    /// edges, but generic enough to support any caller that needs to
    /// enumerate a drawer's outbound graph.
    pub fn outgoing_links(&self, from: u64) -> Result<Vec<(u64, LinkKind)>> {
        let rtxn = self.db.begin_read()?;
        let tbl = rtxn.open_table(LINKS)?;
        let mut out = Vec::new();
        for entry in tbl.iter()? {
            let (k, _) = entry?;
            let key = k.value();
            if key.len() != 17 {
                continue;
            }
            let f = u64::from_le_bytes([
                key[0], key[1], key[2], key[3], key[4], key[5], key[6], key[7],
            ]);
            if f != from {
                continue;
            }
            let to = u64::from_le_bytes([
                key[8], key[9], key[10], key[11], key[12], key[13], key[14], key[15],
            ]);
            if let Some(kind) = LinkKind::from_tag(key[16]) {
                out.push((to, kind));
            }
        }
        Ok(out)
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
    ///   2. Full-scan substring search for the basename in drawer text.
    ///      Palace corpora are small (~10³ drawers); a direct scan keeps
    ///      the code simple now that trigrams no longer exist for drawer
    ///      text. If scale becomes a concern, the BM25 token index could
    ///      be reused for basename-token candidate narrowing.
    ///
    /// Results are deduped and ordered by importance desc, updated_at
    /// desc (R-902).
    pub fn drawers_for_file(&self, file_path: &str) -> Result<Vec<Drawer>> {
        use std::collections::BTreeSet;
        let mut candidates: BTreeSet<u64> = BTreeSet::new();

        // R-1023: resolve the query path to canonical-relative form.
        // Strategy:
        //   * absolute inside canonical_root → strip prefix
        //   * relative → assume cwd-relative; canonicalize if possible,
        //     then strip; fall back to the raw input on failure
        // We also keep the raw `file_path` as a secondary lookup key so
        // callers that already passed a project-relative string still hit.
        let canonical = self.canonical_root()?;
        let resolved = resolve_query_path(canonical.as_deref(), file_path);
        let resolved_str = resolved.to_string_lossy().into_owned();

        // Direct source_file match — try resolved form first, then raw
        // input, deduped via the BTreeSet.
        for key in [resolved_str.as_str(), file_path] {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(FILE_DRAWER_XREF)?;
            let fetched: Vec<u8> = tbl
                .get(key)?
                .map(|v| v.value().to_vec())
                .unwrap_or_default();
            for id in decode_u64_list(&fetched) {
                candidates.insert(id);
            }
        }

        // Basename substring mention across the full corpus.
        let basename = std::path::Path::new(file_path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file_path.to_string());
        if !basename.is_empty() {
            let rtxn = self.db.begin_read()?;
            let drawers_tbl = rtxn.open_table(DRAWERS)?;
            for entry in drawers_tbl.iter()? {
                let (k, v) = entry?;
                let id = k.value();
                if candidates.contains(&id) {
                    continue;
                }
                let d: Drawer = serde_json::from_slice(v.value())?;
                if d.text.contains(&basename) {
                    candidates.insert(id);
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

    /// Drop every BM25 table and re-tokenize every drawer, then stamp
    /// schema v3 metadata (canonical_root + project-relative
    /// `source_file`). Does not touch embeddings. Used by
    /// `ndx recall rebuild-index`. Idempotent across v1, v2, v3 palaces.
    pub fn rebuild_bm25_index(&self) -> Result<u64> {
        // Snapshot drawer texts first (read-only) so the rebuild write
        // txn does not race with concurrent inserts.
        let drawers: Vec<(u64, String)> = {
            let rtxn = self.db.begin_read()?;
            let tbl = rtxn.open_table(DRAWERS)?;
            let mut out = Vec::new();
            for entry in tbl.iter()? {
                let (k, v) = entry?;
                let d: Drawer = serde_json::from_slice(v.value())?;
                out.push((k.value(), d.text));
            }
            out
        };

        let mut count: u64 = 0;
        // Rebuild in MINE_BATCH_SIZE chunks so the first chunk can wipe
        // state and subsequent chunks append.
        let chunks: Vec<_> = drawers.chunks(MINE_BATCH_SIZE).collect();
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let txn = self.db.begin_write()?;
            if chunk_idx == 0 {
                // Truncate all four BM25 tables.
                {
                    let mut posts = txn.open_table(BM25_POSTINGS)?;
                    let keys: Vec<String> = posts
                        .iter()?
                        .filter_map(|r| r.ok().map(|(k, _)| k.value().to_string()))
                        .collect();
                    for k in keys {
                        posts.remove(k.as_str())?;
                    }
                }
                {
                    let mut rev = txn.open_table(DRAWERS_BY_TOKEN)?;
                    let keys: Vec<u64> = rev
                        .iter()?
                        .filter_map(|r| r.ok().map(|(k, _)| k.value()))
                        .collect();
                    for k in keys {
                        rev.remove(k)?;
                    }
                }
                {
                    let mut lengths = txn.open_table(DRAWER_LENGTHS)?;
                    let keys: Vec<u64> = lengths
                        .iter()?
                        .filter_map(|r| r.ok().map(|(k, _)| k.value()))
                        .collect();
                    for k in keys {
                        lengths.remove(k)?;
                    }
                }
                {
                    let mut meta = txn.open_table(BM25_META)?;
                    meta.insert("N", 0u64.to_le_bytes().as_slice())?;
                    meta.insert("total_length", 0u64.to_le_bytes().as_slice())?;
                }
            }
            for (id, text) in chunk.iter() {
                add_drawer_to_bm25(&txn, *id, text)?;
                count += 1;
            }
            txn.commit()?;
        }

        // ── v3 migration ──
        // (a) Stamp canonical_root if missing. The palace's `project_root`
        //     is the absolute path passed to `open_for_migration` — for a
        //     direct (non-symlinked) palace this is the canonical root.
        // (b) Rewrite every drawer's `source_file` against canonical_root.
        //     Paths that were already inside the root become project-
        //     relative; paths outside are left absolute; already-relative
        //     paths pass through. Drawers with no source_file skip.
        // (c) Stamp schema_version = v3 at the end so strict opens succeed.
        //
        // Each leg is idempotent: re-running on a v3 palace makes no
        // changes beyond re-touching the schema_version entry.
        let canonical_root: PathBuf = {
            let txn = self.db.begin_write()?;
            let existing: Option<PathBuf> = read_canonical_root(&txn)?;
            let root = match existing {
                Some(r) => r,
                None => {
                    let abs = absolute_path(&self.project_root);
                    let s = abs
                        .to_str()
                        .context("canonical_root must be valid UTF-8")?
                        .to_string();
                    let mut meta = txn.open_table(META)?;
                    meta.insert("canonical_root", s.as_bytes())?;
                    PathBuf::from(s)
                }
            };
            txn.commit()?;
            root
        };

        // Rewrite source_file entries in drawers + rebuild FILE_DRAWER_XREF
        // under the normalized form. Done in a single pass so the xref
        // table stays consistent with the DRAWERS rows.
        {
            let rtxn = self.db.begin_read()?;
            let drawers_tbl = rtxn.open_table(DRAWERS)?;
            let updates: Vec<(u64, Drawer, Option<String>, Option<String>)> = {
                let mut out = Vec::new();
                for entry in drawers_tbl.iter()? {
                    let (k, v) = entry?;
                    let id = k.value();
                    let mut d: Drawer = serde_json::from_slice(v.value())?;
                    let old = d.source_file.clone();
                    if let Some(ref sf) = d.source_file {
                        let normalized = normalize_source_file(
                            &canonical_root,
                            Path::new(sf),
                        );
                        let norm_str = normalized.to_string_lossy().into_owned();
                        if Some(&norm_str) != old.as_ref() {
                            d.source_file = Some(norm_str);
                        }
                    }
                    let new = d.source_file.clone();
                    if old != new {
                        out.push((id, d, old, new));
                    }
                }
                out
            };
            drop(drawers_tbl);
            drop(rtxn);

            if !updates.is_empty() {
                for chunk in updates.chunks(MINE_BATCH_SIZE) {
                    let txn = self.db.begin_write()?;
                    {
                        let mut drawers_w = txn.open_table(DRAWERS)?;
                        for (id, d, _, _) in chunk {
                            let bytes = serde_json::to_vec(d)?;
                            drawers_w.insert(*id, bytes.as_slice())?;
                        }
                    }
                    // Keep FILE_DRAWER_XREF consistent: remove the id from
                    // its old key, re-insert under the new key.
                    for (id, _, old, new) in chunk {
                        if let Some(old_key) = old {
                            remove_from_string_index(
                                &txn,
                                FILE_DRAWER_XREF,
                                old_key,
                                *id,
                            )?;
                        }
                        if let Some(new_key) = new {
                            add_to_string_index(
                                &txn,
                                FILE_DRAWER_XREF,
                                new_key,
                                *id,
                            )?;
                        }
                    }
                    txn.commit()?;
                }
            }
        }

        // Stamp current schema version so subsequent opens via the strict
        // path succeed. Safe to run on an already-current palace too.
        {
            let txn = self.db.begin_write()?;
            {
                let mut meta = txn.open_table(META)?;
                meta.insert(
                    "schema_version",
                    SCHEMA_VERSION.to_le_bytes().as_slice(),
                )?;
            }
            txn.commit()?;
        }

        Ok(count)
    }

    /// MVCC-backed point-in-time copy of the palace to `target_path`
    /// (R-1052). Opens a read txn on `self`, creates a fresh redb at
    /// the target, and walks every schema table under the read
    /// snapshot, inserting each entry into a single write txn on the
    /// target. Concurrent writers on `self` do not block and cannot
    /// corrupt the copy.
    ///
    /// The target path must not already exist. Callers should pass a
    /// staging path (e.g. `.ndx/recall.redb.new`) and atomically rename
    /// on success.
    pub fn mvcc_copy_to(&self, target_path: &Path) -> Result<()> {
        if target_path.exists() {
            anyhow::bail!(
                "mvcc_copy_to: target {} already exists",
                target_path.display()
            );
        }
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating {}", parent.display())
            })?;
        }

        let target_db = Database::create(target_path)
            .with_context(|| format!("creating {}", target_path.display()))?;

        // Preopen every table on the target so the write txn has a pinned
        // schema, matching the layout in `open_or_create`.
        {
            let txn = target_db.begin_write()?;
            txn.open_table(DRAWERS)?;
            txn.open_table(DRAWER_BY_HASH)?;
            txn.open_table(DRAWER_EMBEDDINGS)?;
            txn.open_table(DRAWERS_BY_ROOM)?;
            txn.open_table(ROOMS)?;
            txn.open_table(BM25_POSTINGS)?;
            txn.open_table(DRAWERS_BY_TOKEN)?;
            txn.open_table(DRAWER_LENGTHS)?;
            txn.open_table(BM25_META)?;
            txn.open_table(LINKS)?;
            txn.open_table(FILE_DRAWER_XREF)?;
            txn.open_table(SESSION_DRAWER_XREF)?;
            txn.open_table(COMMIT_DRAWER_XREF)?;
            txn.open_table(WAKE_INJECTED)?;
            txn.open_table(MINED_SESSIONS)?;
            txn.open_table(META)?;
            txn.commit()?;
        }

        let rtxn = self.db.begin_read()?;
        let wtxn = target_db.begin_write()?;

        // ── string-keyed u64 tables ──
        copy_table_str_u64(&rtxn, &wtxn, WAKE_INJECTED)?;
        copy_table_str_u64(&rtxn, &wtxn, MINED_SESSIONS)?;

        // ── u64-keyed bytes tables ──
        copy_table_u64_bytes(&rtxn, &wtxn, DRAWERS)?;
        copy_table_u64_bytes(&rtxn, &wtxn, DRAWER_EMBEDDINGS)?;
        copy_table_u64_bytes(&rtxn, &wtxn, DRAWERS_BY_TOKEN)?;

        // ── u64-keyed u32 table ──
        copy_table_u64_u32(&rtxn, &wtxn, DRAWER_LENGTHS)?;

        // ── bytes-keyed u64 (hash binding) ──
        copy_table_bytes_u64(&rtxn, &wtxn, DRAWER_BY_HASH)?;

        // ── bytes-keyed bytes (links) ──
        copy_table_bytes_bytes(&rtxn, &wtxn, LINKS)?;

        // ── string-keyed bytes tables ──
        copy_table_str_bytes(&rtxn, &wtxn, DRAWERS_BY_ROOM)?;
        copy_table_str_bytes(&rtxn, &wtxn, ROOMS)?;
        copy_table_str_bytes(&rtxn, &wtxn, BM25_POSTINGS)?;
        copy_table_str_bytes(&rtxn, &wtxn, BM25_META)?;
        copy_table_str_bytes(&rtxn, &wtxn, FILE_DRAWER_XREF)?;
        copy_table_str_bytes(&rtxn, &wtxn, SESSION_DRAWER_XREF)?;
        copy_table_str_bytes(&rtxn, &wtxn, COMMIT_DRAWER_XREF)?;
        copy_table_str_bytes(&rtxn, &wtxn, META)?;

        wtxn.commit()?;
        drop(rtxn);
        drop(target_db);
        Ok(())
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
    let existing: Vec<u8>;
    {
        let fetched = t.get(key)?.map(|v| v.value().to_vec());
        existing = fetched.unwrap_or_default();
    }
    let mut ids = decode_u64_list(&existing);
    if !ids.contains(&id) {
        ids.push(id);
        ids.sort_unstable();
        ids.dedup();
    }
    let bytes = encode_u64_list(&ids);
    t.insert(key, bytes.as_slice())?;
    Ok(())
}

fn remove_from_room_index(
    txn: &redb::WriteTransaction,
    room: &str,
    id: u64,
) -> Result<()> {
    remove_from_string_index(txn, DRAWERS_BY_ROOM, room, id)
}

fn remove_from_string_index(
    txn: &redb::WriteTransaction,
    table_def: TableDefinition<&'static str, &'static [u8]>,
    key: &str,
    id: u64,
) -> Result<()> {
    let mut t = txn.open_table(table_def)?;
    let existing: Vec<u8>;
    {
        let fetched = t.get(key)?.map(|v| v.value().to_vec());
        existing = fetched.unwrap_or_default();
    }
    if existing.is_empty() {
        return Ok(());
    }
    let mut ids = decode_u64_list(&existing);
    ids.retain(|x| *x != id);
    if ids.is_empty() {
        t.remove(key)?;
    } else {
        t.insert(key, encode_u64_list(&ids).as_slice())?;
    }
    Ok(())
}

// ── MVCC copy helpers (R-1052) ──
//
// Each helper walks a table under a read txn and inserts every entry
// into the matching table on the target write txn. Keys and values are
// copied verbatim — no translation, no schema change. The helpers are
// split by table key/value type because redb's `TableDefinition` is a
// phantom-typed handle, and these are the exact type tuples used by the
// palace schema.

fn copy_table_str_bytes(
    rtxn: &redb::ReadTransaction,
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<&'static str, &'static [u8]>,
) -> Result<()> {
    let src = rtxn.open_table(def)?;
    let mut dst = wtxn.open_table(def)?;
    for entry in src.iter()? {
        let (k, v) = entry?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

fn copy_table_str_u64(
    rtxn: &redb::ReadTransaction,
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<&'static str, u64>,
) -> Result<()> {
    let src = rtxn.open_table(def)?;
    let mut dst = wtxn.open_table(def)?;
    for entry in src.iter()? {
        let (k, v) = entry?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

fn copy_table_u64_bytes(
    rtxn: &redb::ReadTransaction,
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<u64, &'static [u8]>,
) -> Result<()> {
    let src = rtxn.open_table(def)?;
    let mut dst = wtxn.open_table(def)?;
    for entry in src.iter()? {
        let (k, v) = entry?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

fn copy_table_u64_u32(
    rtxn: &redb::ReadTransaction,
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<u64, u32>,
) -> Result<()> {
    let src = rtxn.open_table(def)?;
    let mut dst = wtxn.open_table(def)?;
    for entry in src.iter()? {
        let (k, v) = entry?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

fn copy_table_bytes_u64(
    rtxn: &redb::ReadTransaction,
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<&'static [u8], u64>,
) -> Result<()> {
    let src = rtxn.open_table(def)?;
    let mut dst = wtxn.open_table(def)?;
    for entry in src.iter()? {
        let (k, v) = entry?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

fn copy_table_bytes_bytes(
    rtxn: &redb::ReadTransaction,
    wtxn: &redb::WriteTransaction,
    def: TableDefinition<&'static [u8], &'static [u8]>,
) -> Result<()> {
    let src = rtxn.open_table(def)?;
    let mut dst = wtxn.open_table(def)?;
    for entry in src.iter()? {
        let (k, v) = entry?;
        dst.insert(k.value(), v.value())?;
    }
    Ok(())
}

/// Read the `canonical_root` META entry from inside an open write txn.
/// Returns `None` on pre-v3 palaces (the field is absent until
/// `rebuild-index` stamps it).
fn read_canonical_root(txn: &redb::WriteTransaction) -> Result<Option<PathBuf>> {
    let meta = txn.open_table(META)?;
    let out = meta
        .get("canonical_root")?
        .and_then(|v| String::from_utf8(v.value().to_vec()).ok())
        .map(PathBuf::from);
    Ok(out)
}

fn hex_decode_32(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        anyhow::bail!("expected 64 hex chars, got {}", hex.len());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let pair = &hex[i * 2..i * 2 + 2];
        out[i] = u8::from_str_radix(pair, 16)
            .with_context(|| format!("invalid hex at byte {}", i))?;
    }
    Ok(out)
}

/// Tokenize `text`, append this drawer's contribution to every posting
/// list, store its document length and reverse-index row, and bump the
/// corpus meta. Idempotent over token uniqueness only in the sense that
/// callers must call `remove_drawer_from_bm25` first if the drawer was
/// previously indexed.
fn add_drawer_to_bm25(
    txn: &redb::WriteTransaction,
    id: u64,
    text: &str,
) -> Result<()> {
    let tokens = bm25::tokenize(text);
    if tokens.is_empty() {
        // Still record a zero-length row so remove paths find something to
        // decrement if the drawer later gets re-indexed with tokens.
        let mut lengths = txn.open_table(DRAWER_LENGTHS)?;
        lengths.insert(id, 0u32)?;
        let mut rev = txn.open_table(DRAWERS_BY_TOKEN)?;
        rev.insert(id, [].as_slice())?;
        let mut meta = txn.open_table(BM25_META)?;
        let prev_n: u64 = meta
            .get("N")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        meta.insert("N", (prev_n + 1).to_le_bytes().as_slice())?;
        return Ok(());
    }

    let tf = bm25::term_frequencies(&tokens);
    let dl = tokens.len() as u32;
    {
        let mut lengths = txn.open_table(DRAWER_LENGTHS)?;
        lengths.insert(id, dl)?;
        let mut rev = txn.open_table(DRAWERS_BY_TOKEN)?;
        rev.insert(id, encode_token_list(&tokens).as_slice())?;
    }
    {
        let mut posts = txn.open_table(BM25_POSTINGS)?;
        for (tok, count) in tf {
            let existing: Vec<u8> = posts
                .get(tok.as_str())?
                .map(|v| v.value().to_vec())
                .unwrap_or_default();
            let mut entries = decode_posting_list(&existing);
            match entries.binary_search_by_key(&id, |(eid, _)| *eid) {
                Ok(pos) => entries[pos].1 = count,
                Err(pos) => entries.insert(pos, (id, count)),
            }
            posts.insert(tok.as_str(), encode_posting_list(&entries).as_slice())?;
        }
    }
    {
        let mut meta = txn.open_table(BM25_META)?;
        let prev_n: u64 = meta
            .get("N")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        let prev_total: u64 = meta
            .get("total_length")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        meta.insert("N", (prev_n + 1).to_le_bytes().as_slice())?;
        meta.insert(
            "total_length",
            (prev_total + dl as u64).to_le_bytes().as_slice(),
        )?;
    }
    Ok(())
}

/// Reverse of `add_drawer_to_bm25`: drop the drawer from every posting
/// list it appears in (using the `DRAWERS_BY_TOKEN` cache), delete its
/// length row, and decrement corpus meta. A no-op if the drawer has no
/// length row (never indexed / already removed).
fn remove_drawer_from_bm25(
    txn: &redb::WriteTransaction,
    id: u64,
) -> Result<()> {
    // Tokens this drawer contributed.
    let tokens: Vec<String> = {
        let tbl = txn.open_table(DRAWERS_BY_TOKEN)?;
        let fetched = tbl.get(id)?.map(|v| v.value().to_vec());
        match fetched {
            Some(bytes) => decode_token_list(&bytes),
            None => return Ok(()),
        }
    };
    let dl: u32 = {
        let tbl = txn.open_table(DRAWER_LENGTHS)?;
        let fetched = tbl.get(id)?.map(|v| v.value());
        fetched.unwrap_or(0)
    };

    // Deduplicate so we visit each posting list once; tokens was stored
    // in document order with duplicates.
    let unique: std::collections::HashSet<String> = tokens.into_iter().collect();
    if !unique.is_empty() {
        let mut posts = txn.open_table(BM25_POSTINGS)?;
        for tok in unique {
            let existing: Vec<u8> = posts
                .get(tok.as_str())?
                .map(|v| v.value().to_vec())
                .unwrap_or_default();
            let mut entries = decode_posting_list(&existing);
            entries.retain(|(eid, _)| *eid != id);
            if entries.is_empty() {
                posts.remove(tok.as_str())?;
            } else {
                posts.insert(tok.as_str(), encode_posting_list(&entries).as_slice())?;
            }
        }
    }
    {
        let mut rev = txn.open_table(DRAWERS_BY_TOKEN)?;
        rev.remove(id)?;
    }
    {
        let mut lengths = txn.open_table(DRAWER_LENGTHS)?;
        lengths.remove(id)?;
    }
    {
        let mut meta = txn.open_table(BM25_META)?;
        let prev_n: u64 = meta
            .get("N")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        let prev_total: u64 = meta
            .get("total_length")?
            .map(|v| u64_from_bytes(v.value()))
            .unwrap_or(0);
        let new_n = prev_n.saturating_sub(1);
        let new_total = prev_total.saturating_sub(dl as u64);
        meta.insert("N", new_n.to_le_bytes().as_slice())?;
        meta.insert("total_length", new_total.to_le_bytes().as_slice())?;
    }
    Ok(())
}

/// Encode a BM25 posting list: pairs of (u64 drawer_id, u32 tf) in
/// little-endian, 12 bytes per entry. Matches the hand-rolled encoding
/// used elsewhere in this module (`encode_u64_list`, etc.) — avoids
/// pulling in bincode for a format that will never leave redb.
fn encode_posting_list(entries: &[(u64, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 12);
    for (id, tf) in entries {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&tf.to_le_bytes());
    }
    out
}

fn decode_posting_list(b: &[u8]) -> Vec<(u64, u32)> {
    b.chunks_exact(12)
        .map(|c| {
            let id = u64::from_le_bytes([
                c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
            ]);
            let tf = u32::from_le_bytes([c[8], c[9], c[10], c[11]]);
            (id, tf)
        })
        .collect()
}

fn encode_token_list(tokens: &[String]) -> Vec<u8> {
    // `<u32 len><bytes>` repeated. Tokens contain no NUL.
    let mut out = Vec::new();
    for t in tokens {
        let bytes = t.as_bytes();
        let len = bytes.len() as u32;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

fn decode_token_list(mut b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        b = &b[4..];
        if b.len() < len {
            break;
        }
        let tok = String::from_utf8_lossy(&b[..len]).into_owned();
        out.push(tok);
        b = &b[len..];
    }
    out
}


/// Truncate a `String` in place at the nearest char boundary at or
/// before `max_bytes`. Avoids panics from `String::truncate` when the
/// byte offset falls inside a multi-byte UTF-8 character.
pub fn safe_truncate(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Return the longest prefix of `s` that fits within `max_bytes` without
/// splitting a multi-byte character. Safe replacement for `&s[..n]`.
pub fn safe_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

/// Best-effort absolute path. Uses `canonicalize` when the path exists,
/// otherwise joins CWD for relative inputs and leaves absolutes alone.
/// Never fails — falls back to the input on error paths.
pub fn absolute_path(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

/// Return the resolved absolute target of `path` as a string, iff
/// `path` is itself a symlink (or becomes one after reading). Returns
/// `None` when `path` is a regular file, missing, or its metadata is
/// inaccessible. Uses `symlink_metadata` so the call never follows the
/// link (that would defeat the check).
pub fn symlink_resolved_target(path: &Path) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            // Resolve to an absolute target. First try `canonicalize`
            // (follows the chain). If it fails (broken link), fall back
            // to `read_link` joined with the parent directory so status
            // still has a useful value.
            if let Ok(target) = std::fs::canonicalize(path) {
                return Some(target.to_string_lossy().into_owned());
            }
            if let Ok(link) = std::fs::read_link(path) {
                let resolved = if link.is_absolute() {
                    link
                } else {
                    path.parent().unwrap_or(Path::new("")).join(link)
                };
                return Some(resolved.to_string_lossy().into_owned());
            }
            None
        }
        _ => None,
    }
}

/// Resolve a user-supplied path for xref lookup against stored
/// canonical-relative `source_file` values (R-1023).
///
/// Returns the input as a [`PathBuf`] with the canonical_root prefix
/// stripped when applicable. Input is treated as:
///   * absolute → strip canonical_root prefix if present, else keep
///   * relative → try `canonicalize` (cwd-relative), strip prefix;
///     otherwise keep as-is
pub fn resolve_query_path(canonical_root: Option<&Path>, input: &str) -> PathBuf {
    let p = Path::new(input);
    let abs = absolute_path(p);
    match canonical_root {
        Some(root) => normalize_source_file(root, &abs),
        None => {
            if p.is_absolute() {
                abs
            } else {
                p.to_path_buf()
            }
        }
    }
}

/// Normalize a drawer `source_file` for storage per R-1021:
///
/// - If `input` is absolute and lives inside `canonical_root`, return the
///   canonical-relative portion (forward-slash normalized).
/// - If `input` is relative, return it verbatim — callers are expected
///   to pass canonical-relative paths in that case.
/// - If `input` is absolute but outside `canonical_root`, return it
///   unchanged (absolute paths outside the project are preserved).
///
/// The function performs no I/O; both arguments are compared as-is.
pub fn normalize_source_file(canonical_root: &Path, input: &Path) -> PathBuf {
    if !input.is_absolute() {
        return input.to_path_buf();
    }
    match input.strip_prefix(canonical_root) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => input.to_path_buf(),
    }
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
    fn delete_drawer_cascades_across_tables() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room("arch", None, None).unwrap();

        let d = Drawer {
            id: 0,
            text: "switched database engine to postgres".into(),
            content_hash: String::new(),
            room: "arch".to_string(),
            wing: None,
            importance: 7,
            source_kind: SourceKind::Project,
            source_session_id: Some("sess-1".into()),
            source_file: Some("src/db.rs".into()),
            source_line: Some(42),
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        let out = p.insert_drawer_no_embedding(d).unwrap();
        assert!(!out.deduped);
        let id = out.id;

        // Link to a sibling.
        let d2 = Drawer {
            id: 0,
            text: "previously used sqlite".into(),
            content_hash: String::new(),
            room: "arch".to_string(),
            wing: None,
            importance: 5,
            source_kind: SourceKind::Project,
            source_session_id: Some("sess-1".into()),
            source_file: Some("src/db.rs".into()),
            source_line: Some(10),
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        };
        let id2 = p.insert_drawer_no_embedding(d2).unwrap().id;
        p.link_drawers(id, id2, LinkKind::Supersedes).unwrap();

        assert_eq!(p.drawers_for_file("src/db.rs").unwrap().len(), 2);
        assert_eq!(p.drawers_for_session("sess-1").unwrap().len(), 2);
        assert!(p.drawer_ids_in_room_public("arch").unwrap().contains(&id));
        assert!(p.is_superseded(id2).unwrap());

        // Delete id and verify cascade.
        assert!(p.delete_drawer(id).unwrap());
        assert!(p.get_drawer(id).unwrap().is_none());
        assert_eq!(
            p.drawers_for_file("src/db.rs").unwrap().len(),
            1,
            "file xref should drop the deleted drawer"
        );
        assert_eq!(
            p.drawers_for_session("sess-1").unwrap().len(),
            1,
            "session xref should drop the deleted drawer"
        );
        assert!(
            !p.drawer_ids_in_room_public("arch").unwrap().contains(&id),
            "room index should not list the deleted drawer"
        );
        assert!(
            !p.is_superseded(id2).unwrap(),
            "link should be removed in both directions"
        );

        // Re-inserting the same text succeeds (content-hash binding was freed).
        let d3 = Drawer {
            id: 0,
            text: "switched database engine to postgres".into(),
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
        let re = p.insert_drawer_no_embedding(d3).unwrap();
        assert!(!re.deduped, "should be a fresh insert, not a dedup");
    }

    #[test]
    fn update_drawer_changes_room_and_importance() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room("new-room", None, None).unwrap();
        let d = Drawer {
            id: 0,
            text: "some content".into(),
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
        let id = p.insert_drawer_no_embedding(d).unwrap().id;
        let updated = p.update_drawer(id, Some("new-room"), Some(8), None).unwrap();
        assert_eq!(updated.room, "new-room");
        assert_eq!(updated.importance, 8);
        // Room index updated on both sides.
        assert!(!p
            .drawer_ids_in_room_public(UNCLASSIFIED_ROOM)
            .unwrap()
            .contains(&id));
        assert!(p
            .drawer_ids_in_room_public("new-room")
            .unwrap()
            .contains(&id));
    }

    #[test]
    fn pending_classify_and_score_filter() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room("arch", None, None).unwrap();

        // classify candidate: unclassified
        let a = p.insert_drawer_no_embedding(Drawer {
            id: 0, text: "a".into(), content_hash: String::new(),
            room: UNCLASSIFIED_ROOM.into(), wing: None, importance: 5,
            source_kind: SourceKind::Memory, source_session_id: None,
            source_file: None, source_line: None, source_commit: None,
            created_at: 0, updated_at: 0, metadata: BTreeMap::new(),
        }).unwrap();
        // score candidate: classified, default importance, Memory source
        let b = p.insert_drawer_no_embedding(Drawer {
            id: 0, text: "b".into(), content_hash: String::new(),
            room: "arch".into(), wing: None, importance: 5,
            source_kind: SourceKind::Memory, source_session_id: None,
            source_file: None, source_line: None, source_commit: None,
            created_at: 0, updated_at: 0, metadata: BTreeMap::new(),
        }).unwrap();
        // not in either: Manual source at default importance (excluded from score)
        let c = p.insert_drawer_no_embedding(Drawer {
            id: 0, text: "c".into(), content_hash: String::new(),
            room: "arch".into(), wing: None, importance: 5,
            source_kind: SourceKind::Manual, source_session_id: None,
            source_file: None, source_line: None, source_commit: None,
            created_at: 0, updated_at: 0, metadata: BTreeMap::new(),
        }).unwrap();

        let classify = p.list_pending(PendingOp::Classify, 10).unwrap();
        assert_eq!(classify.len(), 1);
        assert_eq!(classify[0].id, a.id);

        // Score candidates: both `a` and `b` have default importance + Memory
        // source. `c` is excluded because its source is Manual (skill contract
        // R-732: don't rescore manually-set drawers).
        let score = p.list_pending(PendingOp::Score, 10).unwrap();
        let score_ids: std::collections::HashSet<u64> = score.iter().map(|d| d.id).collect();
        assert_eq!(score.len(), 2);
        assert!(score_ids.contains(&a.id));
        assert!(score_ids.contains(&b.id));

        // c is neither pending classify nor score.
        assert!(classify.iter().all(|d| d.id != c.id));
        assert!(score.iter().all(|d| d.id != c.id));
    }

    #[test]
    fn wake_injection_state_round_trip() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        assert!(!p.wake_injection_seen("sess-1").unwrap());
        p.mark_wake_injected("sess-1").unwrap();
        assert!(p.wake_injection_seen("sess-1").unwrap());
        // Repeat mark is idempotent.
        p.mark_wake_injected("sess-1").unwrap();
        assert!(p.wake_injection_seen("sess-1").unwrap());
        // Second session unaffected.
        assert!(!p.wake_injection_seen("sess-2").unwrap());
        // Clearing specific session works.
        assert!(p.clear_wake_injection("sess-1").unwrap());
        assert!(!p.wake_injection_seen("sess-1").unwrap());
        // Clear-all on empty table is a no-op.
        p.mark_wake_injected("sess-1").unwrap();
        p.mark_wake_injected("sess-2").unwrap();
        assert_eq!(p.clear_all_wake_injections().unwrap(), 2);
        assert!(!p.wake_injection_seen("sess-1").unwrap());
        assert!(!p.wake_injection_seen("sess-2").unwrap());
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

    fn mk_drawer(text: &str) -> Drawer {
        Drawer {
            id: 0,
            text: text.into(),
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
        }
    }

    #[test]
    fn bm25_indexes_on_insert_and_meta_tracks_corpus() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();

        let a = p
            .insert_drawer_no_embedding(mk_drawer("switched database engine to postgres"))
            .unwrap();
        let b = p
            .insert_drawer_no_embedding(mk_drawer("postgres replaced sqlite"))
            .unwrap();
        let _c = p
            .insert_drawer_no_embedding(mk_drawer("unrelated note about caching"))
            .unwrap();

        // Corpus meta reflects three documents.
        let (n, total, avg) = p.bm25_corpus_stats().unwrap();
        assert_eq!(n, 3);
        assert!(total > 0);
        assert!(avg > 0.0);

        // "postgres" appears in exactly two docs (a, b).
        let posts = p.bm25_postings("postgres").unwrap();
        let ids: std::collections::HashSet<u64> = posts.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&a.id));
        assert!(ids.contains(&b.id));
        assert_eq!(ids.len(), 2);

        // Stopword "to" must have been dropped at tokenization time.
        assert!(p.bm25_postings("to").unwrap().is_empty());
    }

    #[test]
    fn bm25_cascades_on_delete() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();

        let a = p
            .insert_drawer_no_embedding(mk_drawer("rust async runtime tokio"))
            .unwrap();
        let b = p
            .insert_drawer_no_embedding(mk_drawer("rust compiler diagnostics"))
            .unwrap();

        let (n_before, total_before, _) = p.bm25_corpus_stats().unwrap();
        assert_eq!(n_before, 2);
        let rust_before = p.bm25_postings("rust").unwrap();
        assert_eq!(rust_before.len(), 2);

        assert!(p.delete_drawer(a.id).unwrap());

        let (n_after, total_after, _) = p.bm25_corpus_stats().unwrap();
        assert_eq!(n_after, 1, "N decremented");
        assert!(
            total_after < total_before,
            "total_length decreased"
        );
        let rust_after = p.bm25_postings("rust").unwrap();
        assert_eq!(rust_after.len(), 1);
        assert_eq!(rust_after[0].0, b.id);
        // The token 'tokio' was unique to the deleted drawer, its posting
        // list should be empty.
        assert!(p.bm25_postings("tokio").unwrap().is_empty());
    }

    #[test]
    fn bm25_rebuilds_on_text_update() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        let a = p
            .insert_drawer_no_embedding(mk_drawer("apple banana cherry"))
            .unwrap();
        assert_eq!(p.bm25_postings("apple").unwrap().len(), 1);

        p.update_drawer(a.id, None, None, Some("durian elderberry")).unwrap();

        // Old tokens are gone; new tokens are indexed.
        assert!(p.bm25_postings("apple").unwrap().is_empty());
        assert!(p.bm25_postings("banana").unwrap().is_empty());
        assert_eq!(p.bm25_postings("durian").unwrap().len(), 1);
        assert_eq!(p.bm25_postings("elderberry").unwrap().len(), 1);

        // Corpus stats remain consistent (still one document).
        let (n, _, _) = p.bm25_corpus_stats().unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn rebuild_bm25_index_recovers_state() {
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.insert_drawer_no_embedding(mk_drawer("alpha beta gamma"))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("beta gamma delta"))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("gamma delta epsilon"))
            .unwrap();

        let count = p.rebuild_bm25_index().unwrap();
        assert_eq!(count, 3);

        // After rebuild, corpus stats and postings agree with the
        // insert-time state (idempotent).
        let (n, _, _) = p.bm25_corpus_stats().unwrap();
        assert_eq!(n, 3);
        assert_eq!(p.bm25_postings("gamma").unwrap().len(), 3);
        assert_eq!(p.bm25_postings("alpha").unwrap().len(), 1);
    }

    #[test]
    fn open_for_migration_bumps_stale_schema_version() {
        let dir = tmp_project();
        let root = dir.path().canonicalize().unwrap();
        {
            let p = Palace::create_at(root.clone()).unwrap();
            p.insert_drawer_no_embedding(mk_drawer("alpha beta"))
                .unwrap();
            // Simulate a v1 palace by rewriting the stored schema_version
            // AND dropping canonical_root so rebuild-index has to stamp
            // it on the way back up to v3 (covers both migration legs).
            let txn = p.db.begin_write().unwrap();
            {
                let mut meta = txn.open_table(META).unwrap();
                meta.insert("schema_version", 1u32.to_le_bytes().as_slice())
                    .unwrap();
                meta.remove("canonical_root").unwrap();
            }
            txn.commit().unwrap();
        }

        // Strict open rejects a stale version.
        let err = match Palace::open_at(root.clone()) {
            Ok(_) => panic!("open_at should reject stale schema"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("rebuild-index"), "{}", err);

        // Migration open + rebuild stamps the current version AND
        // canonical_root (v1→v3 in one call).
        let p = Palace::open_for_migration(root.clone()).unwrap();
        p.rebuild_bm25_index().unwrap();
        drop(p);

        // Subsequent strict open succeeds with canonical_root populated.
        let p = Palace::open_at(root.clone()).unwrap();
        assert_eq!(p.stats().unwrap().schema_version, SCHEMA_VERSION);
        let stamped = p.canonical_root().unwrap().expect("canonical_root stamped");
        assert_eq!(stamped, root);
    }

    // ── v0.8.0 / Shared Palaces (§19) ──

    #[test]
    fn normalize_source_file_paths() {
        use std::path::PathBuf;
        let root = PathBuf::from("/workspace/proj");
        // Absolute inside canonical_root → stripped.
        assert_eq!(
            normalize_source_file(&root, &PathBuf::from("/workspace/proj/src/a.rs")),
            PathBuf::from("src/a.rs")
        );
        // Nested path.
        assert_eq!(
            normalize_source_file(&root, &PathBuf::from("/workspace/proj/docs/specs/recall.md")),
            PathBuf::from("docs/specs/recall.md")
        );
        // Equal to canonical_root (strip_prefix yields empty) → ".".
        assert_eq!(
            normalize_source_file(&root, &PathBuf::from("/workspace/proj")),
            PathBuf::from(".")
        );
        // Absolute outside canonical_root → left absolute.
        assert_eq!(
            normalize_source_file(&root, &PathBuf::from("/etc/hosts")),
            PathBuf::from("/etc/hosts")
        );
        // Relative input passes through verbatim.
        assert_eq!(
            normalize_source_file(&root, &PathBuf::from("src/a.rs")),
            PathBuf::from("src/a.rs")
        );
        // Trailing-slash variant on canonical_root still works via
        // strip_prefix semantics (PathBuf compares components, not bytes).
        let root_slash = PathBuf::from("/workspace/proj/");
        assert_eq!(
            normalize_source_file(&root_slash, &PathBuf::from("/workspace/proj/src/a.rs")),
            PathBuf::from("src/a.rs")
        );
    }

    #[test]
    fn rebuild_index_v2_to_v3_stamps_canonical_root() {
        let dir = tmp_project();
        let root = dir.path().canonicalize().unwrap();

        // Create a palace, insert drawers with absolute source_file paths
        // that point inside the root. Then simulate a v2 palace by
        // removing canonical_root from META and bumping schema_version
        // back to 2.
        let abs_file = root.join("src").join("a.rs").to_string_lossy().into_owned();
        {
            let p = Palace::create_at(root.clone()).unwrap();
            // Before we stage a v2 state, wipe canonical_root so
            // insert_drawer does NOT pre-normalize the path.
            let txn = p.db.begin_write().unwrap();
            {
                let mut meta = txn.open_table(META).unwrap();
                meta.remove("canonical_root").unwrap();
                meta.insert("schema_version", 2u32.to_le_bytes().as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();

            let mut d = mk_drawer("contents of a");
            d.source_file = Some(abs_file.clone());
            p.insert_drawer_no_embedding(d).unwrap();

            // A second drawer with a path OUTSIDE the root — rebuild
            // must leave it absolute.
            let mut d2 = mk_drawer("outside note");
            d2.source_file = Some("/etc/somefile".to_string());
            p.insert_drawer_no_embedding(d2).unwrap();

            // A third drawer with no source_file — must pass through.
            p.insert_drawer_no_embedding(mk_drawer("no source")).unwrap();
        }

        // v2 → strict open refuses with a pointer to rebuild-index.
        let err = match Palace::open_at(root.clone()) {
            Ok(_) => panic!("open_at should reject v2 palace"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("rebuild-index"), "{}", err);

        // Migration open + rebuild does the v2→v3 work in one call.
        let p = Palace::open_for_migration(root.clone()).unwrap();
        p.rebuild_bm25_index().unwrap();
        drop(p);

        let p = Palace::open_at(root.clone()).unwrap();
        let stats = p.stats().unwrap();
        assert_eq!(stats.schema_version, SCHEMA_VERSION);
        assert_eq!(stats.canonical_root.as_deref(), root.to_str());

        // Verify source_file was rewritten:
        //   - absolute-inside → project-relative
        //   - absolute-outside → unchanged
        //   - None → unchanged
        let all = p.list_drawers(None, 100, 0).unwrap();
        let by_text: std::collections::HashMap<String, Option<String>> = all
            .iter()
            .map(|d| (d.text.clone(), d.source_file.clone()))
            .collect();
        assert_eq!(
            by_text.get("contents of a").unwrap().as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            by_text.get("outside note").unwrap().as_deref(),
            Some("/etc/somefile")
        );
        assert!(by_text.get("no source").unwrap().is_none());

        // FILE_DRAWER_XREF rebuilt on the canonical form.
        let hits = p.drawers_for_file("src/a.rs").unwrap();
        assert!(hits.iter().any(|d| d.text == "contents of a"));
    }

    #[test]
    fn rebuild_index_is_idempotent_on_v3() {
        let dir = tmp_project();
        let root = dir.path().canonicalize().unwrap();
        let p = Palace::create_at(root.clone()).unwrap();
        let mut d = mk_drawer("x");
        d.source_file = Some(root.join("a.txt").to_string_lossy().into_owned());
        p.insert_drawer_no_embedding(d).unwrap();
        drop(p);

        let p = Palace::open_for_migration(root.clone()).unwrap();
        p.rebuild_bm25_index().unwrap();
        let first_root = p.canonical_root().unwrap();
        let first_drawer = p.list_drawers(None, 10, 0).unwrap()[0].clone();
        drop(p);

        // Second run makes no observable change.
        let p = Palace::open_for_migration(root.clone()).unwrap();
        p.rebuild_bm25_index().unwrap();
        assert_eq!(p.canonical_root().unwrap(), first_root);
        let second_drawer = p.list_drawers(None, 10, 0).unwrap()[0].clone();
        assert_eq!(first_drawer.source_file, second_drawer.source_file);
        assert_eq!(p.stats().unwrap().schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn link_palace_rejects_non_empty_without_force() {
        use crate::{cmd_recall_link_palace_at, cmd_recall_unlink_palace_at};
        let canonical_dir = tmp_project();
        let canonical = canonical_dir.path().canonicalize().unwrap();
        let secondary_dir = tmp_project();
        let secondary = secondary_dir.path().canonicalize().unwrap();

        // Canonical has drawers.
        let p_can = Palace::create_at(canonical.clone()).unwrap();
        p_can.insert_drawer_no_embedding(mk_drawer("canonical note"))
            .unwrap();
        drop(p_can);

        // Secondary also has drawers.
        let p_sec = Palace::create_at(secondary.clone()).unwrap();
        p_sec.insert_drawer_no_embedding(mk_drawer("secondary note"))
            .unwrap();
        drop(p_sec);

        let canonical_arg = canonical.to_string_lossy().into_owned();
        let args = vec![canonical_arg.clone()];
        let err = cmd_recall_link_palace_at(&args, &secondary).unwrap_err();
        assert!(err.to_string().contains("drawers"), "{}", err);

        // --force succeeds, creating the symlink.
        let args = vec![canonical_arg.clone(), "--force".into()];
        cmd_recall_link_palace_at(&args, &secondary).unwrap();
        let local = secondary.join(".ndx").join("recall.redb");
        let meta = std::fs::symlink_metadata(&local).unwrap();
        assert!(meta.file_type().is_symlink());

        // Opening the secondary now sees the canonical's drawer.
        let p = Palace::open_at(secondary.clone()).unwrap();
        assert_eq!(p.stats().unwrap().drawer_count, 1);
        drop(p);

        // unlink-palace removes the symlink.
        cmd_recall_unlink_palace_at(&[], &secondary).unwrap();
        assert!(!local.exists());
    }

    #[test]
    fn link_palace_refuses_missing_target() {
        use crate::cmd_recall_link_palace_at;
        let secondary_dir = tmp_project();
        let secondary = secondary_dir.path().canonicalize().unwrap();
        let bogus = secondary.join("does-not-exist");
        let args = vec![bogus.to_string_lossy().into_owned()];
        let err = cmd_recall_link_palace_at(&args, &secondary).unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "{}",
            err
        );
    }

    #[test]
    fn link_palace_resolves_symlink_chain() {
        use crate::cmd_recall_link_palace_at;
        let a_dir = tmp_project();
        let a = a_dir.path().canonicalize().unwrap();
        let b_dir = tmp_project();
        let b = b_dir.path().canonicalize().unwrap();
        let c_dir = tmp_project();
        let c = c_dir.path().canonicalize().unwrap();

        // A is canonical with a drawer.
        let p = Palace::create_at(a.clone()).unwrap();
        p.insert_drawer_no_embedding(mk_drawer("a note")).unwrap();
        drop(p);

        // B → A.
        cmd_recall_link_palace_at(&[a.to_string_lossy().into_owned()], &b).unwrap();

        // C → B should collapse to C → A (R-1043).
        cmd_recall_link_palace_at(&[b.to_string_lossy().into_owned()], &c).unwrap();

        let c_symlink = c.join(".ndx").join("recall.redb");
        let c_target = std::fs::read_link(&c_symlink).unwrap();
        let a_db = a.join(".ndx").join("recall.redb");
        assert_eq!(c_target, a_db, "C must link directly to A, not via B");
    }

    #[test]
    fn unlink_palace_keep_mvcc_copy() {
        use crate::{cmd_recall_link_palace_at, cmd_recall_unlink_palace_at};
        let canonical_dir = tmp_project();
        let canonical = canonical_dir.path().canonicalize().unwrap();
        let secondary_dir = tmp_project();
        let secondary = secondary_dir.path().canonicalize().unwrap();

        // Canonical starts with one drawer.
        let p_can = Palace::create_at(canonical.clone()).unwrap();
        p_can.insert_drawer_no_embedding(mk_drawer("canonical drawer"))
            .unwrap();
        drop(p_can);

        // Link secondary.
        cmd_recall_link_palace_at(
            &[canonical.to_string_lossy().into_owned()],
            &secondary,
        )
        .unwrap();

        // Insert another drawer via the symlinked palace — it should
        // land in the canonical redb.
        let p = Palace::open_at(secondary.clone()).unwrap();
        p.insert_drawer_no_embedding(mk_drawer("via-secondary drawer"))
            .unwrap();
        drop(p);

        // unlink-palace --keep → MVCC copy replaces the symlink.
        cmd_recall_unlink_palace_at(&["--keep".to_string()], &secondary).unwrap();

        let local = secondary.join(".ndx").join("recall.redb");
        let meta = std::fs::symlink_metadata(&local).unwrap();
        assert!(
            !meta.file_type().is_symlink(),
            "after --keep, local palace must be a regular file"
        );

        // The copy has every drawer.
        let p = Palace::open_at(secondary.clone()).unwrap();
        assert_eq!(p.stats().unwrap().drawer_count, 2);
        let texts: std::collections::HashSet<String> = p
            .list_drawers(None, 100, 0)
            .unwrap()
            .into_iter()
            .map(|d| d.text)
            .collect();
        assert!(texts.contains("canonical drawer"));
        assert!(texts.contains("via-secondary drawer"));
    }

    #[test]
    fn status_includes_canonical_root_and_linked_to() {
        use crate::cmd_recall_link_palace_at;
        let canonical_dir = tmp_project();
        let canonical = canonical_dir.path().canonicalize().unwrap();
        let secondary_dir = tmp_project();
        let secondary = secondary_dir.path().canonicalize().unwrap();

        // Canonical palace: stats.canonical_root set, linked_to None.
        let p_can = Palace::create_at(canonical.clone()).unwrap();
        let stats = p_can.stats().unwrap();
        assert_eq!(stats.canonical_root.as_deref(), canonical.to_str());
        assert!(stats.palace_linked_to.is_none());
        drop(p_can);

        // Link B → A.
        cmd_recall_link_palace_at(
            &[canonical.to_string_lossy().into_owned()],
            &secondary,
        )
        .unwrap();

        let p_sec = Palace::open_at(secondary.clone()).unwrap();
        let stats = p_sec.stats().unwrap();
        // canonical_root is inherited from the linked target.
        assert_eq!(stats.canonical_root.as_deref(), canonical.to_str());
        // palace_linked_to is populated because the local redb is a symlink.
        let linked = stats
            .palace_linked_to
            .expect("linked_to populated when palace file is a symlink");
        let canonical_db = canonical.join(".ndx").join("recall.redb");
        let canonical_db_canon = canonical_db.canonicalize().unwrap();
        assert_eq!(
            std::path::PathBuf::from(&linked).canonicalize().unwrap(),
            canonical_db_canon
        );
    }

    #[test]
    fn rehome_rewrites_canonical_root() {
        let dir = tmp_project();
        let root = dir.path().canonicalize().unwrap();
        let p = Palace::create_at(root.clone()).unwrap();
        let original = p.canonical_root().unwrap().unwrap();
        assert_eq!(original, root);

        let new_root = std::path::PathBuf::from("/tmp/relocated-canonical");
        p.set_canonical_root(&new_root).unwrap();

        let after = p.canonical_root().unwrap().unwrap();
        // absolute_path is best-effort; on a nonexistent path it returns
        // the input (absolute case).
        assert_eq!(after, new_root);
        // `rehome` must not re-normalize source_file entries — we're
        // asserting no drawers got mutated (there are none; just verify
        // schema_version is untouched).
        assert_eq!(p.stats().unwrap().schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn xref_drawer_canonical_absolute_input() {
        let dir = tmp_project();
        let root = dir.path().canonicalize().unwrap();
        let p = Palace::create_at(root.clone()).unwrap();

        // Insert via absolute path — normalization at insert stores it
        // canonically-relative.
        let abs = root.join("src").join("main.rs").to_string_lossy().into_owned();
        let mut d = mk_drawer("main.rs content line");
        d.source_file = Some(abs.clone());
        p.insert_drawer_no_embedding(d).unwrap();

        // Verify stored form is project-relative.
        let stored = p.list_drawers(None, 10, 0).unwrap()[0]
            .source_file
            .clone()
            .unwrap();
        assert_eq!(stored, "src/main.rs");

        // Lookup by absolute path must hit (R-1023).
        let hits = p.drawers_for_file(&abs).unwrap();
        assert!(
            hits.iter().any(|d| d.source_file.as_deref() == Some("src/main.rs")),
            "absolute input must resolve via canonical_root"
        );

        // Lookup by project-relative path also hits.
        let hits_rel = p.drawers_for_file("src/main.rs").unwrap();
        assert!(hits_rel
            .iter()
            .any(|d| d.source_file.as_deref() == Some("src/main.rs")));
    }

    #[test]
    fn l3_lexical_ranks_by_bm25() {
        use crate::recall::search::{search, SearchMode};
        let dir = tmp_project();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();

        // Doc A: rare term appears twice in a short doc → should rank high.
        let a = p
            .insert_drawer_no_embedding(mk_drawer("tokio tokio runtime"))
            .unwrap();
        // Doc B: rare term once in a longer doc.
        let b = p
            .insert_drawer_no_embedding(mk_drawer(
                "tokio runtime provides asynchronous primitives for networking",
            ))
            .unwrap();
        // Doc C: no match at all.
        let _c = p
            .insert_drawer_no_embedding(mk_drawer("unrelated filesystem notes"))
            .unwrap();
        // Docs D/E: common term only — shouldn't outrank A or B for "tokio".
        p.insert_drawer_no_embedding(mk_drawer("runtime benchmarks"))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("runtime configuration knobs"))
            .unwrap();

        let hits = search(&p, "tokio", SearchMode::Lexical, None, 10).unwrap();
        // At minimum, A and B appear and A outranks B (higher tf, shorter doc).
        let ids: Vec<u64> = hits.iter().map(|h| h.drawer.id).collect();
        let pos_a = ids.iter().position(|id| *id == a.id);
        let pos_b = ids.iter().position(|id| *id == b.id);
        assert!(pos_a.is_some(), "doc A should be in hits");
        assert!(pos_b.is_some(), "doc B should be in hits");
        assert!(
            pos_a.unwrap() < pos_b.unwrap(),
            "doc A (tf=2, shorter) should outrank doc B (tf=1, longer)"
        );
    }
}
