//! Per-project issue tracker built on the recall palace.
//!
//! Issues are drawers in the reserved [`ISSUES_ROOM`]; their structured
//! state (`open`/`closed`, `closed_at`, `milestone`) lives in the
//! drawer's `metadata` map under namespaced keys so future issue
//! attributes can be added without schema migrations.
//!
//! The CLI surface is `ndx issue add | list | show | close | reopen |
//! update | rm | milestones`. All filtering happens in this module so
//! the underlying `Palace` API stays issue-agnostic.
//!
//! See the design notes in
//! `~/.claude/plans/1-lgtm-2-yes-unified-cocoa.md` (Batch 5) for why
//! this lives on top of drawer machinery rather than in a parallel
//! `issues` table.

use crate::recall::{
    now_unix, Drawer, DrawerInsertOutcome, LinkKind, Palace, RecallError, SourceKind,
    DEFAULT_IMPORTANCE,
};
use anyhow::Result;
use serde::Serialize;

/// Reserved room name for issues.
pub const ISSUES_ROOM: &str = "_issues_";

/// Metadata key for issue status (`"open"` | `"closed"`).
pub const META_STATUS: &str = "issue.status";

/// Metadata key for closed-at unix seconds (string-encoded).
pub const META_CLOSED_AT: &str = "issue.closed_at";

/// Metadata key for milestone tag (free-form).
pub const META_MILESTONE: &str = "issue.milestone";

/// Status values written under [`META_STATUS`].
pub const STATUS_OPEN: &str = "open";
pub const STATUS_CLOSED: &str = "closed";

/// Status filter for [`list_issues`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Closed,
    All,
}

impl StatusFilter {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Inputs for `add`. Title becomes the drawer text first line; body
/// (when present) follows after a blank line.
pub struct AddOptions<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub milestone: Option<&'a str>,
    pub importance: u8,
    pub source_file: Option<&'a str>,
    pub link_drawers: &'a [u64],
}

/// Inputs for `close`.
pub struct CloseOptions<'a> {
    pub fix: Option<&'a str>,
    pub commit: Option<&'a str>,
    /// When `Some`, create a `derived_from` link from the issue back
    /// to this drawer (typically the closing session's main drawer).
    pub link_drawer: Option<u64>,
}

/// Per-milestone summary for `ndx issue milestones`.
#[derive(Debug, Serialize)]
pub struct MilestoneCount {
    pub milestone: String,
    pub open: usize,
    pub closed: usize,
}

/// Build the drawer text for an issue from a title and optional body.
fn compose_text(title: &str, body: Option<&str>) -> String {
    match body {
        Some(b) if !b.is_empty() => format!("{}\n\n{}", title, b),
        _ => title.to_string(),
    }
}

/// File a new open issue. Returns the underlying drawer outcome.
pub fn add(palace: &Palace, opts: AddOptions<'_>) -> Result<DrawerInsertOutcome> {
    if opts.title.trim().is_empty() {
        return Err(RecallError::usage("issue title must not be empty").into());
    }

    let mut drawer = Drawer {
        id: 0,
        text: compose_text(opts.title, opts.body),
        content_hash: String::new(),
        room: ISSUES_ROOM.to_string(),
        wing: None,
        importance: opts.importance,
        source_kind: SourceKind::Manual,
        source_session_id: None,
        source_file: opts.source_file.map(|s| s.to_string()),
        source_line: None,
        source_commit: None,
        created_at: 0,
        updated_at: 0,
        metadata: std::collections::BTreeMap::new(),
    };
    drawer
        .metadata
        .insert(META_STATUS.to_string(), STATUS_OPEN.to_string());
    if let Some(ms) = opts.milestone {
        if !ms.is_empty() {
            drawer
                .metadata
                .insert(META_MILESTONE.to_string(), ms.to_string());
        }
    }

    let outcome = palace.insert_drawer(drawer)?;
    for to in opts.link_drawers {
        // Soft-fail on link insertion — a bad reference shouldn't kill
        // issue creation.
        let _ = palace.link_drawers(outcome.id, *to, LinkKind::References);
    }
    Ok(outcome)
}

/// Close an open issue. Sets `issue.status=closed`, stamps
/// `issue.closed_at`, and appends a structured trailer to the drawer
/// text capturing fix and commit if provided. Optional `link_drawer`
/// records a `derived_from` edge from the issue to the closing
/// drawer (typically the session's most recent rationale drawer).
pub fn close(palace: &Palace, id: u64, opts: CloseOptions<'_>) -> Result<Drawer> {
    let now = now_unix();
    let date = format_date(now);
    let trailer = build_close_trailer(&date, opts.fix, opts.commit);
    palace.append_drawer_text(id, &trailer)?;
    let drawer = palace.patch_drawer_metadata(
        id,
        &[
            (META_STATUS.to_string(), Some(STATUS_CLOSED.to_string())),
            (META_CLOSED_AT.to_string(), Some(now.to_string())),
        ],
    )?;
    if let Some(to) = opts.link_drawer {
        // Soft-fail to keep close idempotent in the face of stale ids.
        let _ = palace.link_drawers(id, to, LinkKind::DerivedFrom);
    }
    Ok(drawer)
}

/// Reopen a closed issue. Clears `issue.closed_at` and resets
/// `issue.status=open`. The close trailer in the drawer text is left
/// in place as a historical record.
pub fn reopen(palace: &Palace, id: u64) -> Result<Drawer> {
    palace.patch_drawer_metadata(
        id,
        &[
            (META_STATUS.to_string(), Some(STATUS_OPEN.to_string())),
            (META_CLOSED_AT.to_string(), None),
        ],
    )
}

/// Set or clear the milestone on an issue.
pub fn set_milestone(palace: &Palace, id: u64, milestone: Option<&str>) -> Result<Drawer> {
    let value = match milestone {
        Some(m) if !m.is_empty() => Some(m.to_string()),
        _ => None,
    };
    palace.patch_drawer_metadata(id, &[(META_MILESTONE.to_string(), value)])
}

/// List issues filtered by status and (optionally) milestone.
pub fn list(
    palace: &Palace,
    status: StatusFilter,
    milestone: Option<&str>,
) -> Result<Vec<Drawer>> {
    let mut all = palace.list_drawers(Some(ISSUES_ROOM), usize::MAX, 0)?;
    all.retain(|d| match_filters(d, status, milestone));
    // Sort: open first by importance desc; within status, newest first.
    all.sort_by(|a, b| {
        let a_open = drawer_status(a) == STATUS_OPEN;
        let b_open = drawer_status(b) == STATUS_OPEN;
        b_open
            .cmp(&a_open)
            .then(b.importance.cmp(&a.importance))
            .then(b.created_at.cmp(&a.created_at))
    });
    Ok(all)
}

/// Aggregate open/closed counts per milestone (and a "(none)" bucket
/// for issues without one).
pub fn milestone_summary(palace: &Palace) -> Result<Vec<MilestoneCount>> {
    let all = palace.list_drawers(Some(ISSUES_ROOM), usize::MAX, 0)?;
    let mut buckets: std::collections::BTreeMap<String, MilestoneCount> =
        std::collections::BTreeMap::new();
    for d in &all {
        let ms = d
            .metadata
            .get(META_MILESTONE)
            .cloned()
            .unwrap_or_else(|| "(none)".to_string());
        let entry = buckets.entry(ms.clone()).or_insert_with(|| MilestoneCount {
            milestone: ms,
            open: 0,
            closed: 0,
        });
        match drawer_status(d) {
            STATUS_OPEN => entry.open += 1,
            STATUS_CLOSED => entry.closed += 1,
            _ => {}
        }
    }
    Ok(buckets.into_values().collect())
}

/// Inspect an issue's status field as a `&str`. Returns
/// [`STATUS_OPEN`] when the field is missing — defensive, so issues
/// migrated from raw drawers always have a sensible default.
pub fn drawer_status(d: &Drawer) -> &str {
    d.metadata
        .get(META_STATUS)
        .map(String::as_str)
        .unwrap_or(STATUS_OPEN)
}

fn match_filters(d: &Drawer, status: StatusFilter, milestone: Option<&str>) -> bool {
    let s = drawer_status(d);
    let status_ok = match status {
        StatusFilter::Open => s == STATUS_OPEN,
        StatusFilter::Closed => s == STATUS_CLOSED,
        StatusFilter::All => true,
    };
    if !status_ok {
        return false;
    }
    match milestone {
        Some(m) => d
            .metadata
            .get(META_MILESTONE)
            .map(String::as_str)
            == Some(m),
        None => true,
    }
}

fn build_close_trailer(date: &str, fix: Option<&str>, commit: Option<&str>) -> String {
    let mut s = format!("\n\n---\n**Closed {}**", date);
    if let Some(f) = fix {
        if !f.is_empty() {
            s.push_str(" — ");
            s.push_str(f);
        }
    }
    if let Some(c) = commit {
        if !c.is_empty() {
            s.push_str(" Commit: ");
            s.push_str(c);
            s.push('.');
        }
    }
    s.push('\n');
    s
}

fn format_date(unix_secs: i64) -> String {
    use chrono::DateTime;
    DateTime::from_timestamp(unix_secs, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| unix_secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::Palace;

    fn open_palace() -> Palace {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the path stays valid for the test body.
        let path = dir.into_path();
        Palace::create_at(path).unwrap()
    }

    #[test]
    fn add_creates_open_issue_in_issues_room() {
        let p = open_palace();
        let outcome = add(
            &p,
            AddOptions {
                title: "fix the timeout",
                body: Some("happens on staging only"),
                milestone: Some("v0.9.0"),
                importance: 7,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap();

        let drawers = p.list_drawers(Some(ISSUES_ROOM), 10, 0).unwrap();
        assert_eq!(drawers.len(), 1);
        let d = &drawers[0];
        assert_eq!(d.id, outcome.id);
        assert!(d.text.contains("fix the timeout"));
        assert!(d.text.contains("happens on staging only"));
        assert_eq!(drawer_status(d), STATUS_OPEN);
        assert_eq!(d.metadata.get(META_MILESTONE).map(String::as_str), Some("v0.9.0"));
        assert_eq!(d.importance, 7);
    }

    #[test]
    fn add_rejects_empty_title() {
        let p = open_palace();
        let err = add(
            &p,
            AddOptions {
                title: "   ",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        );
        assert!(err.is_err());
    }

    #[test]
    fn close_marks_status_and_appends_trailer() {
        let p = open_palace();
        let id = add(
            &p,
            AddOptions {
                title: "broken thing",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;

        let d = close(
            &p,
            id,
            CloseOptions {
                fix: Some("rolled back the bad migration"),
                commit: Some("abc1234"),
                link_drawer: None,
            },
        )
        .unwrap();

        assert_eq!(drawer_status(&d), STATUS_CLOSED);
        assert!(d.metadata.contains_key(META_CLOSED_AT));
        assert!(d.text.contains("**Closed"));
        assert!(d.text.contains("rolled back the bad migration"));
        assert!(d.text.contains("abc1234"));
    }

    #[test]
    fn reopen_clears_closed_at_and_keeps_trailer_history() {
        let p = open_palace();
        let id = add(
            &p,
            AddOptions {
                title: "flake",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        close(
            &p,
            id,
            CloseOptions {
                fix: Some("retried"),
                commit: None,
                link_drawer: None,
            },
        )
        .unwrap();
        let d = reopen(&p, id).unwrap();
        assert_eq!(drawer_status(&d), STATUS_OPEN);
        assert!(!d.metadata.contains_key(META_CLOSED_AT));
        // Trailer must still be there as audit history.
        assert!(d.text.contains("**Closed"));
    }

    #[test]
    fn set_milestone_round_trip_and_clear() {
        let p = open_palace();
        let id = add(
            &p,
            AddOptions {
                title: "thing",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        let d = set_milestone(&p, id, Some("v1.0")).unwrap();
        assert_eq!(d.metadata.get(META_MILESTONE).map(String::as_str), Some("v1.0"));
        let d = set_milestone(&p, id, Some("")).unwrap();
        assert!(!d.metadata.contains_key(META_MILESTONE));
    }

    #[test]
    fn list_filters_by_status_and_sorts_open_first() {
        let p = open_palace();
        let id_open_lo = add(
            &p,
            AddOptions {
                title: "open low",
                body: None,
                milestone: None,
                importance: 3,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        let id_open_hi = add(
            &p,
            AddOptions {
                title: "open high",
                body: None,
                milestone: None,
                importance: 9,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        let id_closed = add(
            &p,
            AddOptions {
                title: "closed thing",
                body: None,
                milestone: None,
                importance: 8,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        close(
            &p,
            id_closed,
            CloseOptions {
                fix: None,
                commit: None,
                link_drawer: None,
            },
        )
        .unwrap();

        let open_only = list(&p, StatusFilter::Open, None).unwrap();
        let ids: Vec<u64> = open_only.iter().map(|d| d.id).collect();
        // Open issues only — closed excluded; importance desc within status.
        assert_eq!(ids, vec![id_open_hi, id_open_lo]);

        let closed_only = list(&p, StatusFilter::Closed, None).unwrap();
        assert_eq!(closed_only.len(), 1);
        assert_eq!(closed_only[0].id, id_closed);

        let all = list(&p, StatusFilter::All, None).unwrap();
        // Open before closed; within open, by importance desc.
        let order: Vec<u64> = all.iter().map(|d| d.id).collect();
        assert_eq!(order, vec![id_open_hi, id_open_lo, id_closed]);
    }

    #[test]
    fn list_filters_by_milestone() {
        let p = open_palace();
        let in_m = add(
            &p,
            AddOptions {
                title: "in m1",
                body: None,
                milestone: Some("m1"),
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        add(
            &p,
            AddOptions {
                title: "no milestone",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap();
        let hits = list(&p, StatusFilter::All, Some("m1")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, in_m);
    }

    #[test]
    fn milestone_summary_groups_open_and_closed() {
        let p = open_palace();
        let m1_open = add(
            &p,
            AddOptions {
                title: "a",
                body: None,
                milestone: Some("v0.9"),
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        let m1_closed = add(
            &p,
            AddOptions {
                title: "b",
                body: None,
                milestone: Some("v0.9"),
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        let _none = add(
            &p,
            AddOptions {
                title: "c",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        close(
            &p,
            m1_closed,
            CloseOptions {
                fix: None,
                commit: None,
                link_drawer: None,
            },
        )
        .unwrap();

        // Quiet the unused variable warning — m1_open is implicit via summary.
        let _ = m1_open;

        let summary = milestone_summary(&p).unwrap();
        let v09 = summary.iter().find(|m| m.milestone == "v0.9").unwrap();
        assert_eq!(v09.open, 1);
        assert_eq!(v09.closed, 1);
        let none = summary.iter().find(|m| m.milestone == "(none)").unwrap();
        assert_eq!(none.open, 1);
        assert_eq!(none.closed, 0);
    }

    #[test]
    fn close_creates_derived_from_link_when_requested() {
        let p = open_palace();
        let session_drawer = p
            .insert_drawer_no_embedding(Drawer {
                id: 0,
                text: "session rationale".to_string(),
                content_hash: String::new(),
                room: "rationale".to_string(),
                wing: None,
                importance: DEFAULT_IMPORTANCE,
                source_kind: SourceKind::Manual,
                source_session_id: None,
                source_file: None,
                source_line: None,
                source_commit: None,
                created_at: 0,
                updated_at: 0,
                metadata: std::collections::BTreeMap::new(),
            })
            .unwrap();

        let issue_id = add(
            &p,
            AddOptions {
                title: "needs link",
                body: None,
                milestone: None,
                importance: DEFAULT_IMPORTANCE,
                source_file: None,
                link_drawers: &[],
            },
        )
        .unwrap()
        .id;
        close(
            &p,
            issue_id,
            CloseOptions {
                fix: None,
                commit: None,
                link_drawer: Some(session_drawer.id),
            },
        )
        .unwrap();

        let links = p.outgoing_links(issue_id).unwrap();
        assert!(
            links.iter().any(|(_to, kind)| matches!(kind, LinkKind::DerivedFrom)),
            "expected a derived_from link from the issue to the session drawer, got {:?}",
            links
        );
    }
}
