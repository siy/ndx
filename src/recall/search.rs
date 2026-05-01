//! L1 wake-up text generation + L3 hybrid (cosine + BM25 + RRF) search.
//!
//! Implements spec §9.2 (R-511..R-515) and §9.4 (R-530..R-536).
//! L0 rendering lives in `identity`; L2 metadata retrieval is handled by
//! `Palace::list_drawers` (used directly from the CLI).

use crate::recall::{bm25, identity, project_name, Drawer, Palace};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

// ── L3 hybrid search ─────────────────────────────────────────────────

/// Which ranking channels to use for a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Hybrid,
    Semantic,
    Lexical,
}

/// Per-result metadata returned by L3 search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub drawer: Drawer,
    pub score: f32,
    pub rank_semantic: Option<usize>,
    pub rank_lexical: Option<usize>,
    pub similarity: Option<f32>,
}

/// Hyperparameters pinned by spec R-532.
pub const K_SEM: usize = 50;
pub const K_LEX: usize = 50;
pub const RRF_K: f32 = 60.0;
pub const DEFAULT_N_OUT: usize = 10;

pub fn search(
    palace: &Palace,
    query: &str,
    mode: SearchMode,
    room: Option<&str>,
    n_out: usize,
) -> Result<Vec<SearchHit>> {
    // ── Room pre-filter (R-535) ──
    let room_filter: Option<HashSet<u64>> = match room {
        Some(r) => Some(palace.drawer_ids_in_room_public(r)?.into_iter().collect()),
        None => None,
    };

    // ── Semantic channel ──
    let mut sem_rank: HashMap<u64, usize> = HashMap::new();
    let mut sem_sim: HashMap<u64, f32> = HashMap::new();
    if matches!(mode, SearchMode::Hybrid | SearchMode::Semantic) {
        let q_emb = palace.embedder()?.embed_one(query)?;
        let mut scored: Vec<(u64, f32)> = palace
            .iter_embeddings()?
            .into_iter()
            .filter(|(id, _)| match &room_filter {
                Some(set) => set.contains(id),
                None => true,
            })
            .map(|(id, v)| (id, crate::recall::embed::cosine(&q_emb, &v)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (id, sim)) in scored.into_iter().take(K_SEM).enumerate() {
            sem_rank.insert(id, rank);
            sem_sim.insert(id, sim);
        }
    }

    // ── Lexical channel (BM25 over a simple tokenizer) ──
    let mut lex_rank: HashMap<u64, usize> = HashMap::new();
    if matches!(mode, SearchMode::Hybrid | SearchMode::Lexical) {
        let query_tokens = bm25::tokenize(query);
        let unique_tokens: HashSet<String> = query_tokens.into_iter().collect();
        if !unique_tokens.is_empty() {
            let (n_docs, _total, avg_dl) = palace.bm25_corpus_stats()?;
            if n_docs > 0 {
                // Accumulate BM25 score per candidate drawer.
                let mut scores: HashMap<u64, f32> = HashMap::new();
                // Cache document lengths so we pay the read once per candidate.
                let mut doc_lengths: HashMap<u64, u32> = HashMap::new();
                for tok in &unique_tokens {
                    let postings = palace.bm25_postings(tok)?;
                    if postings.is_empty() {
                        continue;
                    }
                    let df = postings.len() as u64;
                    let idf = bm25::idf(n_docs, df);
                    for (id, tf) in postings {
                        if let Some(set) = &room_filter {
                            if !set.contains(&id) {
                                continue;
                            }
                        }
                        let dl = match doc_lengths.get(&id) {
                            Some(v) => *v,
                            None => {
                                let v = palace.drawer_token_length(id)?;
                                doc_lengths.insert(id, v);
                                v
                            }
                        };
                        let s = bm25::term_score(tf, dl, avg_dl, idf);
                        *scores.entry(id).or_insert(0.0) += s;
                    }
                }
                let mut scored: Vec<(u64, f32)> = scores.into_iter().collect();
                // Descending: highest BM25 score first; tiebreak by lower id.
                scored.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.0.cmp(&b.0))
                });
                for (rank, (id, _)) in scored.into_iter().take(K_LEX).enumerate() {
                    lex_rank.insert(id, rank);
                }
            }
        }
    }

    // ── Fusion (R-532) ──
    let mut all_ids: HashSet<u64> = HashSet::new();
    all_ids.extend(sem_rank.keys());
    all_ids.extend(lex_rank.keys());

    let mut scored: Vec<(u64, f32)> = all_ids
        .into_iter()
        .map(|id| {
            let s_sem = sem_rank.get(&id).map(|r| 1.0 / (RRF_K + *r as f32)).unwrap_or(0.0);
            let s_lex = lex_rank.get(&id).map(|r| 1.0 / (RRF_K + *r as f32)).unwrap_or(0.0);
            (id, s_sem + s_lex)
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // ── Hydrate top N ──
    let mut hits = Vec::new();
    for (id, score) in scored.into_iter().take(n_out) {
        if let Some(drawer) = palace.get_drawer(id)? {
            hits.push(SearchHit {
                drawer,
                score,
                rank_semantic: sem_rank.get(&id).copied(),
                rank_lexical: lex_rank.get(&id).copied(),
                similarity: sem_sim.get(&id).copied(),
            });
        }
    }
    Ok(hits)
}

// ── L1 wake-up text ──────────────────────────────────────────────────

const L1_MAX_DRAWERS: usize = 15;
const L1_MAX_CHARS: usize = 3200;
const L1_SNIPPET_MAX: usize = 200;

/// Reserved room name for the always-loaded "Do-Not-Repeat" channel
/// (Batch 4). Drawers in this room are concatenated into wake-up text
/// regardless of their importance, capped by `[wakeup] dnr_cap` in
/// `identity.toml` (default `DNR_DEFAULT_CAP`). Matching the existing
/// `_summary_` precedent for underscore-prefixed reserved rooms.
pub const DNR_ROOM: &str = "_do_not_repeat_";

/// Default cap on the Do-Not-Repeat block when `[wakeup] dnr_cap` is
/// absent from the merged identity. Keeps the always-loaded section
/// from crowding L1 budget on long-running projects.
pub const DNR_DEFAULT_CAP: usize = 20;

/// Read `[wakeup] dnr_cap` from a merged identity TOML, falling back
/// to `DNR_DEFAULT_CAP` for missing / wrong-typed entries. Negative
/// or zero values fall back to the default — a non-positive cap is
/// almost always a misconfiguration.
fn dnr_cap_from(merged: Option<&toml::Value>) -> usize {
    merged
        .and_then(|v| v.get("wakeup"))
        .and_then(|v| v.get("dnr_cap"))
        .and_then(|v| v.as_integer())
        .filter(|n| *n > 0)
        .map(|n| n as usize)
        .unwrap_or(DNR_DEFAULT_CAP)
}

/// Generate L0 + Do-Not-Repeat + L1 wake-up text. Identity is loaded
/// from global + per-project TOML (R-311..R-313); the Do-Not-Repeat
/// channel renders every drawer in `_do_not_repeat_` (capped) above
/// L1; L1 is computed from the top importance-ranked drawers of the
/// current palace (R-511..R-514) excluding the DnR room (those are
/// already shown above).
pub fn wake_up(palace: &Palace) -> Result<String> {
    let mut out = String::new();

    // L0 — identity
    let merged = identity::load_merged(palace.project_root())?;
    let pname = project_name(palace.project_root());
    out.push_str(&identity::render_l0(merged.as_ref(), Some(&pname)));

    out.push('\n');

    // Do-Not-Repeat — always loaded, capped per identity.
    let cap = dnr_cap_from(merged.as_ref());
    let dnr = render_dnr(palace, cap)?;
    if !dnr.is_empty() {
        out.push_str(&dnr);
        out.push('\n');
    }

    // L1 — essential story
    out.push_str(&render_l1(palace)?);

    Ok(out)
}

/// Render the Do-Not-Repeat block. Empty string when the room has no
/// drawers (so wake-up text doesn't carry a useless empty header).
/// Drawers are sorted by importance desc, then created-at desc; if
/// the count exceeds `cap`, an overflow line points at `/ndx-chore`.
pub fn render_dnr(palace: &Palace, cap: usize) -> Result<String> {
    let mut drawers = palace.list_drawers(Some(DNR_ROOM), usize::MAX, 0)?;
    drawers.retain(|d| !palace.is_superseded(d.id).unwrap_or(false));

    if drawers.is_empty() {
        return Ok(String::new());
    }

    drawers.sort_by(|a, b| {
        b.importance
            .cmp(&a.importance)
            .then(b.created_at.cmp(&a.created_at))
    });

    let total = drawers.len();
    let shown = total.min(cap);

    let mut out = String::from("## DO-NOT-REPEAT\n");
    for d in drawers.iter().take(shown) {
        let snippet = compact_snippet(&d.text, L1_SNIPPET_MAX);
        out.push_str(&format!("  - {}\n", snippet));
    }
    if total > shown {
        let extra = total - shown;
        let noun = if extra == 1 { "rule" } else { "rules" };
        out.push_str(&format!(
            "  _({} more {} in {}; run /ndx-chore to consolidate)_\n",
            extra, noun, DNR_ROOM
        ));
    }
    Ok(out)
}

/// Render just the L1 section. Split out so hook wake-up injection and
/// tests can exercise it without the identity machinery.
pub fn render_l1(palace: &Palace) -> Result<String> {
    let mut drawers = collect_l1_candidates(palace)?;

    // Sort by importance desc, created_at desc tiebreak (R-512 step 3).
    drawers.sort_by(|a, b| {
        b.importance
            .cmp(&a.importance)
            .then(b.created_at.cmp(&a.created_at))
    });

    let mut out = String::from("## L1 — ESSENTIAL\n");
    if drawers.is_empty() {
        out.push_str(
            "*(no drawers yet — run `ndx recall mine --from-memory` to seed)*\n",
        );
        return Ok(out);
    }

    // Group by room.
    let mut grouped: BTreeMap<String, Vec<&Drawer>> = BTreeMap::new();
    let mut total_len = out.len();
    let mut used = 0usize;
    for d in &drawers {
        if used >= L1_MAX_DRAWERS {
            break;
        }
        grouped.entry(d.room.clone()).or_default().push(d);
        used += 1;
    }

    let mut truncated = false;
    'outer: for (room, entries) in &grouped {
        let header = format!("\n[{}]\n", room);
        if total_len + header.len() > L1_MAX_CHARS {
            truncated = true;
            break;
        }
        out.push_str(&header);
        total_len += header.len();

        for d in entries {
            let snippet = compact_snippet(&d.text, L1_SNIPPET_MAX);
            let src = source_hint(d);
            let line = if src.is_empty() {
                format!("  - {}\n", snippet)
            } else {
                format!("  - {}  ({})\n", snippet, src)
            };
            if total_len + line.len() > L1_MAX_CHARS {
                truncated = true;
                break 'outer;
            }
            out.push_str(&line);
            total_len += line.len();
        }
    }

    if truncated {
        out.push_str("  … (more in L2 / L3 search)\n");
    }
    Ok(out)
}

fn collect_l1_candidates(palace: &Palace) -> Result<Vec<Drawer>> {
    let all = palace.list_drawers(None, usize::MAX, 0)?;
    let mut out = Vec::with_capacity(all.len());
    for d in all {
        // Drawers in the Do-Not-Repeat room are rendered above L1;
        // skip here to avoid showing them twice.
        if d.room == DNR_ROOM {
            continue;
        }
        if palace.is_superseded(d.id)? {
            continue;
        }
        out.push(d);
    }
    Ok(out)
}

fn compact_snippet(text: &str, max: usize) -> String {
    let single_line: String = text.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if single_line.chars().count() <= max {
        single_line
    } else {
        let mut truncated: String = single_line.chars().take(max.saturating_sub(1)).collect();
        truncated.push('…');
        truncated
    }
}

fn source_hint(d: &Drawer) -> String {
    if let Some(path) = &d.source_file {
        let short = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        return format!("src: {}", short);
    }
    if let Some(sid) = &d.source_session_id {
        return format!("session: {}", crate::recall::safe_prefix(sid, 8));
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::{SourceKind, DEFAULT_IMPORTANCE, UNCLASSIFIED_ROOM};
    use std::collections::BTreeMap;

    fn mk_drawer(text: &str, importance: u8, room: &str) -> Drawer {
        Drawer {
            id: 0,
            text: text.to_string(),
            content_hash: String::new(),
            room: room.to_string(),
            wing: None,
            importance,
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
    fn compact_snippet_truncates_gracefully() {
        let s = compact_snippet("a\nb\nc\nd", 100);
        assert_eq!(s, "a b c d");
        let s = compact_snippet("aaaaaaaaaa", 5);
        assert_eq!(s.chars().count(), 5);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn l1_empty_palace() {
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        let out = render_l1(&p).unwrap();
        assert!(out.contains("## L1 — ESSENTIAL"));
        assert!(out.contains("no drawers yet"));
    }

    #[test]
    fn l1_orders_by_importance_and_groups_by_room() {
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room("decisions", None, None).unwrap();
        p.ensure_room("people", None, None).unwrap();

        p.insert_drawer_no_embedding(mk_drawer("low one", 2, UNCLASSIFIED_ROOM))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("top decision", 9, "decisions"))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("mid fact", 5, "decisions"))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("a person", 7, "people"))
            .unwrap();

        let out = render_l1(&p).unwrap();
        // Top decision should come before the mid one.
        let top_pos = out.find("top decision").unwrap();
        let mid_pos = out.find("mid fact").unwrap();
        assert!(top_pos < mid_pos);
        // Room headers appear.
        assert!(out.contains("[decisions]"));
        assert!(out.contains("[people]"));
        assert!(out.contains("[unclassified]"));
    }

    #[test]
    fn l1_skips_superseded_drawers() {
        use crate::recall::LinkKind;
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        let old = p
            .insert_drawer_no_embedding(mk_drawer(
                "old position",
                8,
                UNCLASSIFIED_ROOM,
            ))
            .unwrap();
        let newer = p
            .insert_drawer_no_embedding(mk_drawer(
                "new position",
                8,
                UNCLASSIFIED_ROOM,
            ))
            .unwrap();
        // old is superseded by newer
        p.link_drawers(newer.id, old.id, LinkKind::Supersedes).unwrap();

        let out = render_l1(&p).unwrap();
        assert!(out.contains("new position"));
        assert!(
            !out.contains("old position"),
            "superseded drawer should be hidden"
        );
    }

    // ── Do-Not-Repeat channel ─────────────────────────────────────────

    #[test]
    fn dnr_empty_room_renders_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        let out = render_dnr(&p, DNR_DEFAULT_CAP).unwrap();
        assert_eq!(out, "", "empty DnR should render no header at all");
    }

    #[test]
    fn dnr_renders_in_importance_desc() {
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room(DNR_ROOM, None, None).unwrap();
        p.insert_drawer_no_embedding(mk_drawer("low rule", 3, DNR_ROOM))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer("hi rule", 9, DNR_ROOM))
            .unwrap();

        let out = render_dnr(&p, DNR_DEFAULT_CAP).unwrap();
        assert!(out.contains("## DO-NOT-REPEAT"));
        let hi = out.find("hi rule").unwrap();
        let lo = out.find("low rule").unwrap();
        assert!(hi < lo, "high importance should render first");
    }

    #[test]
    fn dnr_caps_with_overflow_message() {
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room(DNR_ROOM, None, None).unwrap();
        for i in 0..7u8 {
            p.insert_drawer_no_embedding(mk_drawer(
                &format!("rule {}", i),
                10 - i,
                DNR_ROOM,
            ))
            .unwrap();
        }

        let out = render_dnr(&p, 3).unwrap();
        assert!(out.contains("rule 0")); // top 3 by importance
        assert!(out.contains("rule 1"));
        assert!(out.contains("rule 2"));
        assert!(!out.contains("rule 3"), "rule 3 exceeds cap of 3");
        assert!(
            out.contains("4 more rules"),
            "overflow line should mention remaining count, got: {}",
            out
        );
        // Edge case — singular noun for exactly one overflow rule.
        let out1 = render_dnr(&p, 6).unwrap();
        assert!(
            out1.contains("1 more rule") && !out1.contains("1 more rules"),
            "singular 'rule' should be used when exactly 1 over cap, got: {}",
            out1
        );
        assert!(out.contains(DNR_ROOM));
        assert!(out.contains("/ndx-chore"));
    }

    #[test]
    fn dnr_excluded_from_l1() {
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room(DNR_ROOM, None, None).unwrap();
        p.insert_drawer_no_embedding(mk_drawer("never repeat", 9, DNR_ROOM))
            .unwrap();
        p.insert_drawer_no_embedding(mk_drawer(
            "normal fact",
            8,
            UNCLASSIFIED_ROOM,
        ))
        .unwrap();

        let l1 = render_l1(&p).unwrap();
        assert!(l1.contains("normal fact"));
        assert!(
            !l1.contains("never repeat"),
            "DnR drawers should not duplicate into L1"
        );
    }

    #[test]
    fn dnr_skips_superseded() {
        use crate::recall::LinkKind;
        let dir = tempfile::tempdir().unwrap();
        let p = Palace::create_at(dir.path().to_path_buf()).unwrap();
        p.ensure_room(DNR_ROOM, None, None).unwrap();
        let old = p
            .insert_drawer_no_embedding(mk_drawer("old rule", 9, DNR_ROOM))
            .unwrap();
        let new_d = p
            .insert_drawer_no_embedding(mk_drawer("new rule", 9, DNR_ROOM))
            .unwrap();
        p.link_drawers(new_d.id, old.id, LinkKind::Supersedes)
            .unwrap();

        let out = render_dnr(&p, DNR_DEFAULT_CAP).unwrap();
        assert!(out.contains("new rule"));
        assert!(!out.contains("old rule"));
    }

    // ── Cap parsing ───────────────────────────────────────────────────

    #[test]
    fn dnr_cap_default_when_missing() {
        assert_eq!(dnr_cap_from(None), DNR_DEFAULT_CAP);
        let v: toml::Value = toml::from_str("").unwrap();
        assert_eq!(dnr_cap_from(Some(&v)), DNR_DEFAULT_CAP);
    }

    #[test]
    fn dnr_cap_reads_from_wakeup_table() {
        let v: toml::Value = toml::from_str("[wakeup]\ndnr_cap = 5\n").unwrap();
        assert_eq!(dnr_cap_from(Some(&v)), 5);
    }

    #[test]
    fn dnr_cap_falls_back_on_non_positive() {
        let v: toml::Value = toml::from_str("[wakeup]\ndnr_cap = 0\n").unwrap();
        assert_eq!(dnr_cap_from(Some(&v)), DNR_DEFAULT_CAP);
        let v: toml::Value = toml::from_str("[wakeup]\ndnr_cap = -1\n").unwrap();
        assert_eq!(dnr_cap_from(Some(&v)), DNR_DEFAULT_CAP);
    }

    #[test]
    fn dnr_cap_falls_back_on_wrong_type() {
        let v: toml::Value = toml::from_str("[wakeup]\ndnr_cap = \"big\"\n").unwrap();
        assert_eq!(dnr_cap_from(Some(&v)), DNR_DEFAULT_CAP);
    }
}
