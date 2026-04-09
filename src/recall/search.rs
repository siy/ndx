//! L1 wake-up text generation + L3 hybrid (cosine + trigram + RRF) search.
//!
//! Implements spec §9.2 (R-511..R-515) and §9.4 (R-530..R-536).
//! L0 rendering lives in `identity`; L2 metadata retrieval is handled by
//! `Palace::list_drawers` (used directly from the CLI).

use crate::recall::{
    extract_query_trigrams, identity, project_name, Drawer, Palace,
};
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

    // ── Lexical channel ──
    let mut lex_rank: HashMap<u64, usize> = HashMap::new();
    if matches!(mode, SearchMode::Hybrid | SearchMode::Lexical) {
        let query_tris = extract_query_trigrams(query);
        if !query_tris.is_empty() {
            let mut hit_counts: HashMap<u64, u32> = HashMap::new();
            for tri in &query_tris {
                let ids = palace.trigram_postings(tri)?;
                for id in ids {
                    if let Some(set) = &room_filter {
                        if !set.contains(&id) {
                            continue;
                        }
                    }
                    *hit_counts.entry(id).or_insert(0) += 1;
                }
            }
            let mut scored: Vec<(u64, u32)> = hit_counts.into_iter().collect();
            // Descending: most trigram matches first; tiebreak by lower id.
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (rank, (id, _)) in scored.into_iter().take(K_LEX).enumerate() {
                lex_rank.insert(id, rank);
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

/// Generate L0 + L1 wake-up text. Identity is loaded from global +
/// per-project TOML (R-311..R-313); L1 is computed from the top
/// importance-ranked drawers of the current palace (R-511..R-514).
pub fn wake_up(palace: &Palace) -> Result<String> {
    let mut out = String::new();

    // L0 — identity
    let merged = identity::load_merged(palace.project_root())?;
    let pname = project_name(palace.project_root());
    out.push_str(&identity::render_l0(merged.as_ref(), Some(&pname)));

    out.push('\n');

    // L1 — essential story
    out.push_str(&render_l1(palace)?);

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
}
