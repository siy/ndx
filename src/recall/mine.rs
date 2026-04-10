//! Drawer mining: ingest from global session memory, from a mempalace
//! ChromaDB export, or by walking the project tree.
//!
//! Implements spec §10 (R-600..R-633). All mine modes are idempotent via
//! content-hash dedup in [`Palace::insert_drawer`] (R-102) and commit in
//! batches of [`MINE_BATCH_SIZE`] per write transaction (R-631).

use crate::memory::MemoryIndex;
use crate::recall::{
    Drawer, DrawerInsertOutcome, Palace, SourceKind, DEFAULT_IMPORTANCE,
    MINE_BATCH_SIZE, UNCLASSIFIED_ROOM,
};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Outcome counts for a mine run (R-454).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MineReport {
    pub added: u64,
    pub deduped: u64,
    pub skipped: u64,
}

impl MineReport {
    fn record(&mut self, outcome: DrawerInsertOutcome) {
        if outcome.deduped {
            self.deduped += 1;
        } else {
            self.added += 1;
        }
    }
}

// ── mine --from-memory ──────────────────────────────────────────────────

/// Read sessions from global `~/.ndx/memory.redb`, filter by the palace's
/// project path, walk each session's JSONL file turn-pair by turn-pair,
/// and emit one drawer per pair (R-601..R-605).
///
/// Streams in chunks of [`MINE_BATCH_SIZE`] to bound memory. Skips
/// sessions already mined (by session_id + source_modified) unless
/// `force` is true. Embedding is skipped by default (`embed = false`)
/// for speed; run `ndx recall reembed` or `search` to backfill.
pub fn mine_from_memory(
    palace: &Palace,
    since: Option<&str>,
    force: bool,
    embed: bool,
) -> Result<MineReport> {
    let mem = MemoryIndex::open().context("failed to open global memory database")?;
    let project_path = palace.project_root().to_string_lossy().into_owned();
    let sessions = mem.list_sessions(Some(&project_path), usize::MAX)?;

    let mut report = MineReport::default();
    let mut buf: Vec<Drawer> = Vec::with_capacity(MINE_BATCH_SIZE);
    let mut sessions_processed = 0u64;

    for session in &sessions {
        // Filter by --since (R-605).
        if let Some(min) = since {
            let started = session.started_at.as_deref().unwrap_or("");
            if started < min {
                continue;
            }
        }

        // Skip sessions already mined unless --force.
        if !force
            && palace
                .session_already_mined(&session.session_id, session.source_modified)?
        {
            continue;
        }

        let src = Path::new(&session.source_path);
        if !src.exists() {
            report.skipped += 1;
            continue;
        }

        let pairs = match extract_turn_pairs(src) {
            Ok(p) => p,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };

        for text in pairs {
            buf.push(Drawer {
                id: 0,
                text,
                content_hash: String::new(),
                room: UNCLASSIFIED_ROOM.to_string(),
                wing: None,
                importance: DEFAULT_IMPORTANCE,
                source_kind: SourceKind::Memory,
                source_session_id: Some(session.session_id.clone()),
                source_file: None,
                source_line: None,
                source_commit: None,
                created_at: 0,
                updated_at: 0,
                metadata: BTreeMap::new(),
            });

            // Flush chunk when buffer is full.
            if buf.len() >= MINE_BATCH_SIZE {
                flush_batch(palace, &mut buf, &mut report, embed)?;
            }
        }

        palace.mark_session_mined(&session.session_id, session.source_modified)?;
        sessions_processed += 1;
        eprint!(
            "\rmining: {} drawers from {} sessions...",
            report.added + report.deduped,
            sessions_processed
        );
    }

    // Final flush.
    if !buf.is_empty() {
        flush_batch(palace, &mut buf, &mut report, embed)?;
    }
    if sessions_processed > 0 {
        eprintln!(); // newline after progress
    }

    palace.mark_last_mined()?;
    Ok(report)
}

fn flush_batch(
    palace: &Palace,
    buf: &mut Vec<Drawer>,
    report: &mut MineReport,
    embed: bool,
) -> Result<()> {
    let batch = std::mem::take(buf);
    let outcomes = if embed {
        palace.insert_drawers_batch(batch)?
    } else {
        palace.insert_drawers_batch_no_embed(batch)?
    };
    for o in outcomes {
        report.record(o);
    }
    Ok(())
}

/// Parse a Claude session JSONL file into turn-pair strings.
/// A "turn-pair" is one `human` message followed by the next `assistant`
/// message (R-602). Multiple consecutive humans flush a user-only pair;
/// trailing assistant-only turns are ignored.
fn extract_turn_pairs(path: &Path) -> Result<Vec<String>> {
    let body = std::fs::read_to_string(path)?;
    let mut pairs: Vec<String> = Vec::new();
    let mut pending_user: Option<String> = None;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let node: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let kind = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "user" | "human" => {
                // Flush any previous user-only turn as its own drawer.
                if let Some(u) = pending_user.take() {
                    pairs.push(format_pair(&u, ""));
                }
                pending_user = extract_content(&node).filter(|s| !s.is_empty());
            }
            "assistant" => {
                if let Some(a) = extract_content(&node).filter(|s| !s.is_empty()) {
                    let user = pending_user.take().unwrap_or_default();
                    pairs.push(format_pair(&user, &a));
                }
            }
            _ => {}
        }
    }
    if let Some(u) = pending_user.take() {
        pairs.push(format_pair(&u, ""));
    }
    // Filter: keep only high-signal turns, drop assistant narration noise.
    Ok(pairs.into_iter().filter(|p| is_high_signal_turn(p)).collect())
}

/// Heuristic signal filter for session turn-pairs. Returns true for turns
/// that likely contain decisions, rationale, outcomes, or user corrections.
/// Returns false for mechanical assistant narration ("Let me read...",
/// "Now I'll check...") and trivial user continuations ("ok", "yes").
fn is_high_signal_turn(text: &str) -> bool {
    // Very short turns are almost always noise ("ok", "yes", "go ahead").
    if text.len() < 40 {
        return false;
    }

    // Positive signals: decision/rationale markers in either role.
    const KEEP_MARKERS: &[&str] = &[
        "decided", "chose", "chosen", "because", "rationale", "reason",
        "trade-off", "tradeoff", "instead of", "rather than", "prefer",
        "should", "must", "won't", "don't", "never", "always",
        "switched", "migrated", "replaced", "deprecated", "removed",
        "bug", "fix", "broke", "regression", "root cause",
        "design", "architecture", "pattern", "convention",
        "important", "critical", "requirement", "constraint",
        "learned", "surprised", "mistake", "corrected",
    ];

    let lower = text.to_ascii_lowercase();

    // If it contains a decision/rationale marker, keep it.
    if KEEP_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }

    // Negative signals: assistant narration openers.
    const NOISE_PREFIXES: &[&str] = &[
        "assistant: let me ",
        "assistant: now let me ",
        "assistant: now i'll ",
        "assistant: i'll read ",
        "assistant: i'll check ",
        "assistant: i'll look ",
        "assistant: looking at ",
        "assistant: reading ",
        "assistant: checking ",
        "assistant: running ",
        "assistant: the file ",
        "assistant: here's the ",
        "assistant: here is the ",
    ];

    if NOISE_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return false;
    }

    // Default: keep (err on the side of retention for medium-length turns).
    true
}

fn format_pair(user: &str, assistant: &str) -> String {
    match (user.is_empty(), assistant.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("USER: {}", user),
        (true, false) => format!("ASSISTANT: {}", assistant),
        (false, false) => format!("USER: {}\n\nASSISTANT: {}", user, assistant),
    }
}

fn extract_content(node: &serde_json::Value) -> Option<String> {
    let content = node.get("message").and_then(|m| m.get("content"))?;
    extract_text_blocks(content)
}

fn extract_text_blocks(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            for block in arr {
                if let serde_json::Value::String(s) = block {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(s);
                } else if let Some(text) =
                    block.get("text").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

// ── mine --from-chroma ─────────────────────────────────────────────────

/// Import drawers from a mempalace ChromaDB directory (R-611..R-614).
///
/// ChromaDB ≥0.5 stores data in `chroma.sqlite3`. We read documents and
/// string/int/float metadata via the `embedding_metadata` key-value table,
/// which is present across recent ChromaDB versions. Embeddings are not
/// preserved in Phase 2 (fastembed arrives in Phase 3); drawers imported
/// here will be re-embedded on first `ndx recall reembed`.
pub fn mine_from_chroma(
    palace: &Palace,
    chroma_dir: &Path,
    wing_filter: Option<&str>,
    embed: bool,
) -> Result<MineReport> {
    use rusqlite::{Connection, OpenFlags};

    // Locate chroma.sqlite3 under the given directory, or accept a path
    // that already points at the sqlite file directly.
    let sqlite_path = if chroma_dir.is_file() {
        chroma_dir.to_path_buf()
    } else {
        let candidate = chroma_dir.join("chroma.sqlite3");
        if !candidate.exists() {
            anyhow::bail!(
                "could not find chroma.sqlite3 under {}",
                chroma_dir.display()
            );
        }
        candidate
    };

    let conn = Connection::open_with_flags(
        &sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", sqlite_path.display()))?;

    // Gather all (embedding_id, document_text) + metadata via embedding_metadata.
    // ChromaDB convention: `key='chroma:document'` holds the raw text in
    // `string_value`. Other keys are free-form metadata.
    // We scan the whole table once, group rows by embedding id.
    let mut stmt = conn
        .prepare(
            "SELECT id, key, string_value, int_value, float_value \
             FROM embedding_metadata",
        )
        .context("chroma schema probe: embedding_metadata table not found")?;

    let mut grouped: BTreeMap<i64, ChromaDoc> = BTreeMap::new();
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        let key: String = row.get(1)?;
        let string_value: Option<String> = row.get(2)?;
        let int_value: Option<i64> = row.get(3)?;
        let float_value: Option<f64> = row.get(4)?;
        Ok((id, key, string_value, int_value, float_value))
    })?;

    for row in rows {
        let (id, key, sv, iv, fv) = row?;
        let entry = grouped.entry(id).or_default();
        match key.as_str() {
            "chroma:document" => {
                if let Some(s) = sv {
                    entry.document = Some(s);
                }
            }
            "wing" => {
                if let Some(s) = sv {
                    entry.wing = Some(s);
                }
            }
            "room" => {
                if let Some(s) = sv {
                    entry.room = Some(s);
                }
            }
            "source_file" => {
                if let Some(s) = sv {
                    entry.source_file = Some(s);
                }
            }
            "importance" | "weight" | "emotional_weight" => {
                if let Some(i) = iv {
                    entry.importance = Some(i as f64);
                } else if let Some(f) = fv {
                    entry.importance = Some(f);
                } else if let Some(s) = sv.as_ref().and_then(|s| s.parse::<f64>().ok()) {
                    entry.importance = Some(s);
                }
            }
            _ => {
                if let Some(s) = sv {
                    entry.metadata.insert(key, s);
                } else if let Some(i) = iv {
                    entry.metadata.insert(key, i.to_string());
                } else if let Some(f) = fv {
                    entry.metadata.insert(key, f.to_string());
                }
            }
        }
    }
    drop(stmt);
    drop(conn);

    let mut report = MineReport::default();
    let mut staged: Vec<Drawer> = Vec::new();

    for (_id, doc) in grouped {
        let text = match doc.document {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                report.skipped += 1;
                continue;
            }
        };

        // Wing filter (R-614).
        if let Some(want) = wing_filter {
            let got = doc.wing.as_deref().unwrap_or("");
            if got != want {
                continue;
            }
        }

        let importance = doc
            .importance
            .map(|f| f.round().clamp(1.0, 10.0) as u8)
            .unwrap_or(DEFAULT_IMPORTANCE);

        staged.push(Drawer {
            id: 0,
            text,
            content_hash: String::new(),
            room: doc.room.unwrap_or_else(|| UNCLASSIFIED_ROOM.to_string()),
            wing: doc.wing,
            importance,
            source_kind: SourceKind::Chroma,
            source_session_id: None,
            source_file: doc.source_file,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: doc.metadata,
        });
    }

    let outcomes = if embed {
        palace.insert_drawers_batch(staged)?
    } else {
        palace.insert_drawers_batch_no_embed(staged)?
    };
    for o in outcomes {
        report.record(o);
    }
    palace.mark_last_mined()?;
    Ok(report)
}

#[derive(Debug, Default)]
struct ChromaDoc {
    document: Option<String>,
    wing: Option<String>,
    room: Option<String>,
    source_file: Option<String>,
    importance: Option<f64>,
    metadata: BTreeMap<String, String>,
}

// ── mine --project ─────────────────────────────────────────────────────

/// Walk the project tree, paragraph-chunk text files, and emit one drawer
/// per paragraph (R-621..R-624). Streams in chunks to bound memory.
pub fn mine_project(
    palace: &Palace,
    scan_root: Option<&Path>,
    embed: bool,
) -> Result<MineReport> {
    let root: PathBuf = scan_root
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| palace.project_root().to_path_buf());

    let mut report = MineReport::default();
    let mut buf: Vec<Drawer> = Vec::with_capacity(MINE_BATCH_SIZE);
    let mut files_processed = 0u64;

    let walker = WalkBuilder::new(&root).standard_filters(true).build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_mineable(path) {
            report.skipped += 1;
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };
        if meta.len() > 1024 * 1024 {
            report.skipped += 1;
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                report.skipped += 1;
                continue;
            }
        };

        let rel = path
            .strip_prefix(palace.project_root())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let auto_room = infer_room_from_path(&rel);
        for (text, line) in split_paragraphs(&content) {
            buf.push(Drawer {
                id: 0,
                text,
                content_hash: String::new(),
                room: auto_room.clone(),
                wing: None,
                importance: DEFAULT_IMPORTANCE,
                source_kind: SourceKind::Project,
                source_session_id: None,
                source_file: Some(rel.clone()),
                source_line: Some(line),
                source_commit: None,
                created_at: 0,
                updated_at: 0,
                metadata: BTreeMap::new(),
            });

            if buf.len() >= MINE_BATCH_SIZE {
                flush_batch(palace, &mut buf, &mut report, embed)?;
            }
        }

        files_processed += 1;
        if files_processed % 100 == 0 {
            eprint!(
                "\rmining: {} drawers from {} files...",
                report.added + report.deduped,
                files_processed
            );
        }
    }

    if !buf.is_empty() {
        flush_batch(palace, &mut buf, &mut report, embed)?;
    }
    if files_processed >= 100 {
        eprintln!();
    }

    palace.mark_last_mined()?;
    Ok(report)
}

/// Paragraph splitter: splits on blank-line boundaries, then further splits
/// oversized (>2 KiB) paragraphs at sentence boundaries. Returns the
/// paragraph text and the 1-based line number of its first line (R-622, R-623).
pub fn split_paragraphs(content: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_start: u32 = 1;
    let mut line_num: u32 = 0;

    for line in content.lines() {
        line_num += 1;
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                flush_paragraph(&mut out, std::mem::take(&mut current), current_start);
            }
            current_start = line_num + 1;
            continue;
        }
        if current.is_empty() {
            current_start = line_num;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        flush_paragraph(&mut out, current, current_start);
    }
    out
}

fn flush_paragraph(out: &mut Vec<(String, u32)>, text: String, start: u32) {
    const MAX: usize = 2 * 1024;
    if text.len() <= MAX {
        out.push((text, start));
        return;
    }
    // Split oversized paragraphs at sentence boundaries.
    let mut chunk = String::new();
    for sent in split_sentences(&text) {
        if chunk.len() + sent.len() > MAX && !chunk.is_empty() {
            out.push((std::mem::take(&mut chunk), start));
        }
        chunk.push_str(&sent);
    }
    if !chunk.is_empty() {
        out.push((chunk, start));
    }
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        buf.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Map well-known filenames and directory patterns to rooms so that
/// `mine --project` produces pre-classified drawers instead of dumping
/// everything into `unclassified`. Returns the room name.
fn infer_room_from_path(rel_path: &str) -> String {
    let lower = rel_path.to_ascii_lowercase();
    let basename = std::path::Path::new(rel_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    // Exact filename matches.
    match basename.as_str() {
        "changelog.md" | "changes.md" | "history.md" => return "releases".into(),
        "claude.md" => return "conventions".into(),
        "readme.md" => return "overview".into(),
        "contributing.md" | "code_of_conduct.md" => return "conventions".into(),
        "architecture.md" | "design.md" => return "architecture".into(),
        "security.md" => return "security".into(),
        "license" | "license.md" | "license.txt" => return "overview".into(),
        _ => {}
    }

    // Directory-based inference.
    if lower.starts_with("proposals/") || lower.starts_with("rfcs/") || lower.starts_with("rfc/") {
        return "proposals".into();
    }
    if lower.starts_with("docs/specs/") || lower.starts_with("spec/") || lower.starts_with("specs/") {
        return "architecture".into();
    }
    if lower.starts_with("docs/") || lower.starts_with("doc/") {
        return "documentation".into();
    }

    UNCLASSIFIED_ROOM.into()
}

/// Reject binary files and common noise (R-624).
fn is_mineable(path: &Path) -> bool {
    // Extension denylist.
    let denied_exts = [
        "lock", "map", "pb", "bin", "gz", "zip", "tar", "png", "jpg", "jpeg",
        "gif", "webp", "ico", "pdf", "ttf", "otf", "woff", "woff2", "mp3",
        "mp4", "mov", "wasm", "so", "dylib", "dll", "a", "o", "rlib",
    ];
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if denied_exts.iter().any(|d| *d == ext) {
            return false;
        }
    }
    // Filename patterns.
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if name.ends_with(".min.js") || name == "package-lock.json" || name == "Cargo.lock" {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_split_on_blank_lines() {
        let text = "para one\nstill one\n\npara two\n\n\npara three";
        let p = split_paragraphs(text);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].0, "para one\nstill one");
        assert_eq!(p[0].1, 1);
        assert_eq!(p[1].0, "para two");
        assert_eq!(p[1].1, 4);
        assert_eq!(p[2].0, "para three");
        assert_eq!(p[2].1, 7);
    }

    #[test]
    fn oversized_paragraph_is_split_on_sentences() {
        let big = format!("{} {}", "foo.".repeat(600), "bar!".repeat(600));
        let p = split_paragraphs(&big);
        assert!(p.len() > 1, "oversized paragraph should have been split");
    }

    #[test]
    fn format_pair_variants() {
        assert_eq!(format_pair("q", "a"), "USER: q\n\nASSISTANT: a");
        assert_eq!(format_pair("q", ""), "USER: q");
        assert_eq!(format_pair("", "a"), "ASSISTANT: a");
        assert_eq!(format_pair("", ""), "");
    }

    #[test]
    fn signal_filter_keeps_decisions_drops_narration() {
        assert!(is_high_signal_turn(
            "USER: We decided to use Postgres because of JSONB support"
        ));
        assert!(is_high_signal_turn(
            "USER: switched from REST to GraphQL\n\nASSISTANT: That's a significant architecture change"
        ));
        assert!(!is_high_signal_turn("USER: ok"));
        assert!(!is_high_signal_turn("USER: yes"));
        assert!(!is_high_signal_turn(
            "ASSISTANT: Let me read the file and check."
        ));
    }

    #[test]
    fn auto_room_from_path() {
        assert_eq!(infer_room_from_path("CHANGELOG.md"), "releases");
        assert_eq!(infer_room_from_path("CLAUDE.md"), "conventions");
        assert_eq!(infer_room_from_path("README.md"), "overview");
        assert_eq!(infer_room_from_path("docs/specs/recall.md"), "architecture");
        assert_eq!(infer_room_from_path("proposals/auth-rfc.md"), "proposals");
        assert_eq!(infer_room_from_path("docs/guide.md"), "documentation");
        assert_eq!(infer_room_from_path("src/main.rs"), "unclassified");
    }

    #[test]
    fn denylist_rejects_common_noise() {
        assert!(!is_mineable(Path::new("foo.lock")));
        assert!(!is_mineable(Path::new("app.min.js")));
        assert!(!is_mineable(Path::new("icon.png")));
        assert!(is_mineable(Path::new("src/main.rs")));
        assert!(is_mineable(Path::new("README.md")));
    }
}
