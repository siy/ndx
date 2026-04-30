use anyhow::{Context, Result};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const MANIFEST_INDEX_URL: &str = "https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands/index.txt";
const MANIFEST_BASE_URL: &str = "https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands";

const SKILL_CONTENT: &str = r#"# ndx — Fast File Index, Memory Search, Recall Palace

Use the `ndx` CLI for trigram-accelerated file search, project file listing, session memory queries, cross-referencing, and the per-project **recall palace** (structured episodic memory). ndx is available via Bash and works in all contexts including subagents.

## Lifecycle — when to do what

The palace is **mined automatically** by the SessionStart and SessionEnd hooks; you almost never need to invoke `mine` by hand. Curation, however, is a judgment task and stays explicit:

- **Daily / per session** — Run `/ndx-chore` after a notable mine (or once per week even when quiet). It walks classify → score → dedupe → contradict to completion in one go and prints a per-phase summary.
- **Session-end** — Run `/ndx-recall-handover` before `/exit` to capture durable insights as memory files for the next session. SessionStart's auto-mine cannot recover lessons you didn't write down.
- **Occasional** — Run `/ndx-recall-summarize` after large mining/classification rounds to refresh the per-room summary drawers that surface first in L1 wake-up.
- **Surgical review** — When `/ndx-chore` flags something specific, drop into the focused skill: `/ndx-recall-classify`, `/ndx-recall-score`, `/ndx-recall-dedupe`, `/ndx-recall-contradict`. Each one walks a single pending queue with full rule detail.

If a SessionStart nudge appears in your context (`# ndx-recall — palace hygiene pending`), that's the harness telling you the backlog crossed the threshold — run `/ndx-chore`.

## When to reach for the CLI directly

- **Content search across many files** — faster than grep for large codebases due to the trigram index
- **Session memory** — find what was discussed or done in previous Claude Code sessions
- **Cross-referencing** — find which sessions touched a file, or what files a session modified
- **Recall palace lookup** — `ndx recall search "query"` for hybrid retrieval, `ndx recall wake` for the L0+L1 context block

# Reference

## File Index Commands

All file index commands operate on the project in the current working directory. The first invocation scans and indexes the project (~100ms for 10K files).

### Search file contents
```bash
ndx search <pattern>
ndx search "TODO" --file-pattern "*.rs"
ndx search "fn main" -B 2 -A 5
ndx search "error" --output files        # just file names
ndx search "import" --output count       # match counts per file
ndx search "pattern" --max-results 50 --offset 100  # pagination
```

### List files
```bash
ndx list                                  # all indexed files
ndx list --path src/                      # files under src/
ndx list --pattern "*.rs"                 # filter by glob
ndx list --sort modified                  # newest first
```

### Find files by glob
```bash
ndx find "**/*.toml"
ndx find "src/**/*.rs" --sort modified
```

### Index status
```bash
ndx status
```

## Memory Commands

Search and browse past Claude Code session transcripts and command events.

```bash
ndx memory search "database migration"          # search session transcripts
ndx memory events "docker"                       # search command event log
ndx memory list                                  # recent sessions
ndx memory list --project /path/to/project       # filter by project
ndx memory stats                                 # session/event/agent counts
ndx memory session <session-id>                  # full session details
ndx memory context                               # recent sessions + events for current project
ndx memory subagents "search query"              # search subagent transcripts
ndx memory tree <session-id>                     # session + subagent tree
```

All memory commands accept `--limit N` to control result count.

## Recall Palace Commands

Per-project structured memory (`{project}/.ndx/recall.redb`). Drawers = atomic memory units, grouped into rooms, retrievable via hybrid (semantic + lexical) search.

### Lifecycle
```bash
ndx recall init                            # create the palace
ndx recall init --link <canonical-root>    # link a secondary checkout to a shared palace
ndx recall link-palace <canonical-root>    # replace local palace with a symlink (--force to overwrite drawers)
ndx recall unlink-palace [--keep]          # remove symlink; --keep copies the canonical palace locally first
ndx recall rehome <new-canonical-root>     # rewrite canonical_root in META after canonical checkout moves
ndx recall status [--json]                 # counts, schema, embedding model, canonical root, link target
```

### Mining (fill the palace)
```bash
ndx recall mine --from-memory              # derive drawers from global session memory
ndx recall mine --from-memory --since 2026-01-01
ndx recall mine --from-chroma <path>       # import from a mempalace ChromaDB export
ndx recall mine --project                  # walk current project, paragraph-chunk text files
ndx recall mine --project --path docs/
```

### Retrieval (the 4-layer ladder)
```bash
ndx recall wake                            # L0 identity + L1 top drawers, as prompt text
ndx recall get --room architecture         # L2 metadata retrieval
ndx recall search "query"                  # L3 hybrid (default: semantic + lexical RRF)
ndx recall search "query" --semantic       # semantic only
ndx recall search "query" --lexical        # BM25 only
ndx recall search "query" --room decisions
```

### Drawer CRUD
```bash
ndx recall drawer list [--room X] [--limit N] [--json]
ndx recall drawer show --id N [--json]
ndx recall drawer add "text" [--room X] [--importance N] [--source-file F]
ndx recall drawer update --id N [--room X] [--importance N] [--text "..."]
ndx recall drawer rm --id N
ndx recall drawer link --from A --to B --kind <references|contradicts|supersedes|derived_from>
ndx recall drawer unlink --from A --to B [--kind <kind>]
```

### Rooms & identity
```bash
ndx recall room add <name> --title "..." --description "..."
ndx recall room list
ndx recall room rename <old> <new>
ndx recall room rm <name>
ndx recall identity show [--merged]
ndx recall identity edit [--project]       # opens $EDITOR on identity.toml
```

### Skill-assisted maintenance

The palace ingests everything raw. Quality (room assignment, importance, deduplication, contradiction flagging, summarization) is curated via slash commands:

- `/ndx-chore` — orchestrator: walks classify → score → dedupe → contradict to completion in one go (preferred entry point)
- `/ndx-recall-classify` — assign rooms to `unclassified` drawers
- `/ndx-recall-score` — set meaningful importance on default-5 drawers
- `/ndx-recall-dedupe` — merge near-duplicates
- `/ndx-recall-contradict` — flag contradictions between drawers
- `/ndx-recall-summarize` — produce per-room summary drawers
- `/ndx-recall-handover` — save session insights as memories for the next Claude session

Use `/ndx-chore` as your daily maintenance entry point. Drop into the focused skills only for surgical review.

## Cross-Reference Commands

```bash
ndx xref file src/main.rs                # find sessions that touched this file
ndx xref session <session-id>            # list files touched by a session
ndx xref drawer src/main.rs              # find palace drawers that reference a file
ndx xref drawer-session <session-id>     # drawers derived from a session
ndx xref git <commit>                    # drawers referencing files changed in a commit
```

## Maintenance

```bash
ndx scan              # re-scan project index + memory database
ndx install           # download command manifests, register hooks, install global slash commands to ~/.claude/commands/
ndx init              # wire ndx into the current project (CLAUDE.md, .gitignore); slash commands stay global
ndx init --clean-up   # also remove any pre-existing project-local skill copies (refuses git-tracked files; prints `git rm` hint)
ndx recall reembed    # backfill embeddings (downloads model if needed)
ndx recall rebuild-index  # re-tokenize all drawers into BM25 (v1→v2) and stamp canonical_root + normalize source_file paths (v2→v3)
```
"#;

// ── Recall maintenance skills ──

const SKILL_RECALL_CLASSIFY: &str = r#"# /ndx-recall-classify — Assign rooms to unclassified drawers

Work through the current project's recall palace and assign each unclassified drawer to a meaningful room (topic bucket).

## What to do

1. **Fetch the batch**
   ```bash
   ndx recall drawer list --pending classify --limit 25 --json
   ```
   Returns a JSON object: `{"op": "classify", "project": {"path": ..., "existing_rooms": [...]}, "drawers": [...]}`.
   Each drawer has `id`, `text`, `source_file`, `source_session_id`, and current `room` (will be `"unclassified"`).

2. **Decide the room for each drawer**
   Read each drawer's `text`. Assign the best-fitting room. Prefer existing rooms from `project.existing_rooms` when one fits; create new rooms only when none match.
   Good room names: lowercase, `[a-z0-9_-]+`, ≤64 chars. Examples: `architecture`, `decisions`, `people`, `tools`, `bugs`, `glossary`, `rationale`, `setup`.

3. **Bulk-classify by source file first**
   Before classifying individual drawers, check if whole-file moves save work:
   ```bash
   ndx recall drawer update --source-file CHANGELOG.md --room releases
   ndx recall drawer update --source-file docs/specs/ --room architecture
   ndx recall drawer update --source-file CLAUDE.md --room conventions
   ```
   This moves all drawers from a source file (or path prefix) to a room in one command. Re-fetch the pending batch after bulk moves to see what remains.

4. **Classify remaining drawers individually**
   ```bash
   ndx recall drawer update --id <N> --room <room> --json
   ```
   `drawer update` auto-creates the target room. Optionally add titles:
   ```bash
   ndx recall room add <name> --title "..." --description "..."
   ```

5. **Handle noise**
   If a drawer is pure noise (markdown separator, single punctuation, boilerplate header), delete it instead:
   ```bash
   ndx recall drawer rm --id <N>
   ```

## Stopping criteria

- Every drawer in the fetched batch has `room != "unclassified"`.
- Repeat steps 1-3 until `ndx recall drawer list --pending classify --limit 1 --json` returns an empty `drawers` array.

## Guidelines

- When a drawer is genuinely ambiguous, leave it in `unclassified` and move on — don't invent rooms just to clear the queue.
- Don't split topics that an existing room already covers.
- Prefer fewer rooms with clearer boundaries over a sprawling taxonomy.
- This is a judgment task. If the user has an evident naming convention (from `ndx recall room list`), follow it.
"#;

const SKILL_RECALL_SCORE: &str = r#"# /ndx-recall-score — Assign importance to drawers

Score drawers that still have the default importance (5). Importance is used to rank drawers in the L1 wake-up text and to tiebreak search results.

## What to do

1. **Fetch the batch**
   ```bash
   ndx recall drawer list --pending score --limit 25 --json
   ```
   Returns drawers that currently have `importance == 5` and `source_kind != Manual` (manually-set drawers are excluded from rescoring).

2. **Bulk-score by source file when patterns are clear**
   If all drawers from a source file deserve the same score:
   ```bash
   ndx recall drawer update --source-file CHANGELOG.md --importance 4
   ndx recall drawer update --source-file docs/specs/ --importance 7
   ```

3. **Score remaining drawers individually on a 1-10 scale**
   - **10** — Core identity facts, critical decisions, irreversible constraints. Always load in wake-up.
   - **7-9** — Important decisions, rationale, architectural pillars, people the user collaborates with regularly.
   - **4-6** — Normal context: conversations, code patterns, general project facts.
   - **1-3** — Low-signal noise, incidental output, boilerplate the dedup path amplified by accident.

4. **Apply individual scores**
   ```bash
   ndx recall drawer update --id <N> --importance <1..10> --json
   ```

5. **Downgrade noise aggressively**
   Drawers whose text is a markdown separator, a heading with no body, or repeated boilerplate should be scored 1-2 or deleted:
   ```bash
   ndx recall drawer rm --id <N>
   ```

## Stopping criteria

- Every drawer in the batch has a non-default importance (i.e., you've touched it).
- Repeat until `ndx recall drawer list --pending score --limit 1 --json` returns an empty array.

## Guidelines

- Be stingy with 9-10. The wake-up budget is small; reserve those for things that absolutely must be loaded.
- If you're uncertain between two values, pick the lower one. It's easier to bump up later than to notice over-important noise.
- Score on **content quality**, not length. A one-line "we chose Postgres because of JSONB" can be a 9.
"#;

const SKILL_RECALL_DEDUPE: &str = r#"# /ndx-recall-dedupe — Merge near-duplicate drawers

Find drawers with overlapping content and merge them into a single canonical entry. Raw text mining often produces multiple drawers covering the same decision from different angles; this skill consolidates them.

## What to do

1. **Fetch candidate groups**
   ```bash
   ndx recall drawer list --pending dedupe --limit 20 --json
   ```
   Returns drawers that share content-hash prefixes with at least one other drawer. Each group is a cluster of candidates, not a confirmed duplicate set.

2. **Inspect each group**
   Read the `text` of the candidates in each cluster. Decide:
   - **True duplicates** (same claim, same angle) → keep the highest-importance one, delete the rest, optionally bump the survivor's importance.
   - **Complementary** (same claim, different detail) → merge the details into one drawer by editing its text with `drawer update --text "..."`, then delete the others.
   - **Coincidental hash prefix collision** → skip; they're not actually related.

3. **Apply merges**
   To consolidate drawer B into drawer A:
   ```bash
   ndx recall drawer update --id A --text "merged content" --importance <bumped>
   ndx recall drawer rm --id B
   ```
   When the survivor's text changes, its embedding and BM25 index are regenerated automatically.

4. **Record supersession** (optional, preserves audit trail instead of deleting)
   ```bash
   ndx recall drawer link --from <newer> --to <older> --kind supersedes
   ```
   Superseded drawers are excluded from L1 wake-up but remain queryable via `drawer show` and `search`.

## Stopping criteria

- Every returned group has been inspected.
- Repeat until `ndx recall drawer list --pending dedupe --limit 1 --json` returns an empty array.

## Guidelines

- Be conservative with merges. When in doubt, leave duplicates alone — extra noise is recoverable, lost content is not.
- Prefer `supersedes` links over deletion when the old drawer has historical value.
- Watch for near-duplicates that actually represent a change over time (e.g., "we use Postgres" then "we switched from Postgres to Cockroach"). Those are not duplicates, they're a timeline; link with `supersedes`.
"#;

const SKILL_RECALL_CONTRADICT: &str = r#"# /ndx-recall-contradict — Flag contradictions between drawers

Find and record contradictions between drawers so L1 wake-up can favor the current view and search can surface conflicts for the user to resolve.

## What to do

1. **Fetch candidate pairs**
   ```bash
   ndx recall drawer list --pending contradict --limit 30 --json
   ```
   Returns drawers that already participate in some link. Start from these and widen the search manually via `ndx recall search` if you want a broader sweep.

2. **Identify real contradictions**
   A real contradiction is when two drawers make incompatible claims about the same topic:
   - "auth uses JWT" vs "auth uses session cookies"
   - "deployment runs on K8s" vs "deployment is a single systemd unit"
   - "Alice is the tech lead" vs "Bob is the tech lead"

   Not contradictions:
   - Different topics that share vocabulary
   - Complementary facts ("uses Postgres for OLTP" + "uses Clickhouse for analytics")
   - Sequential decisions (use `supersedes` instead)

3. **Record the contradiction**
   ```bash
   ndx recall drawer link --from <A> --to <B> --kind contradicts
   ```
   Contradict links are symmetric in intent but stored directionally; record both directions if meaningful, or just one.

4. **Optionally resolve**
   If one drawer is clearly correct and the other is stale, supersede instead:
   ```bash
   ndx recall drawer link --from <correct> --to <stale> --kind supersedes
   ```
   This hides the stale drawer from L1 wake-up. If neither is clearly correct, leave the contradict link in place and flag the issue to the user.

## Stopping criteria

- Every candidate pair has been either linked (contradicts or supersedes) or confirmed not a contradiction.
- Repeat the fetch step until the batch is empty.

## Guidelines

- Contradiction flagging is judgment-heavy. When unsure, do nothing — false contradict links add noise, missed ones are recoverable.
- Don't manufacture contradictions by stretching interpretations. If it takes a paragraph of reasoning to see the conflict, it probably isn't one.
- Always report unresolved contradictions to the user at the end of the run.
"#;

const SKILL_RECALL_HANDOVER: &str = r#"# /ndx-recall-handover — Session knowledge handover

Reflect on what you learned during this session and save actionable observations as memories so the next Claude session starts smarter.

## What to do

1. **Review what happened this session**
   Look at the work you did — code changes, decisions made, problems solved, user corrections, things that surprised you.

2. **Identify durable insights**
   Focus on knowledge that will still matter next session:
   - **Mining strategy** — what to mine, what to skip, which paths have signal vs noise
   - **Room taxonomy** — which rooms worked, which were too broad or too narrow, naming conventions
   - **Scoring calibration** — what importance levels felt right for this project's content
   - **Bulk operation shortcuts** — which source files map cleanly to rooms or importance levels (e.g. `--source-file CHANGELOG.md --room releases` saved 90% of classify work)
   - **User preferences** — corrections the user made, patterns they prefer, things they rejected
   - **Project-specific patterns** — architectural decisions, key people, recurring themes, terminology

3. **Save as memories**
   Write each insight as a memory file. Use the appropriate type:
   - `feedback` — for corrections and validated approaches ("user prefers X", "don't do Y because Z")
   - `project` — for project-specific facts ("auth system uses JWT", "Alice owns deploys")
   - `user` — for user preferences and working style

4. **Check the recall palace state**
   ```bash
   ndx recall status
   ndx recall room list
   ```
   Note anything about the palace's current state that would help the next session orient quickly.

5. **Summarize for the user**
   Report what you saved and why — transparency builds trust in the memory system.

## What NOT to save

- Code patterns visible in the codebase (just read the code)
- Git history (use `git log`)
- Anything already in CLAUDE.md
- Ephemeral task state ("currently working on X")
- Obvious facts that any session would discover independently

## Guidelines

- **Be concrete.** "Mining the full book/ directory creates 12K noise fragments" is useful. "Be careful with mining" is not.
- **Include the why.** "Session memory is 80% noise — assistant narration like 'Let me read that file' isn't durable knowledge" is better than "filter session memory."
- **Keep it short.** Each memory should be 3-8 lines. If it needs more, it's probably two memories.
- **Check existing memories first.** Read what's already saved and update rather than duplicate.
"#;

const SKILL_RECALL_SUMMARIZE: &str = r#"# /ndx-recall-summarize — Generate per-room summary drawers

Produce one high-quality summary drawer per active room. Summary drawers are stored in a `_summary_` room with max importance (10) so they surface first in L1 wake-up text.

## What to do

1. **Fetch room representatives**
   ```bash
   ndx recall drawer list --pending summarize --limit 20 --json
   ```
   Returns one representative drawer per non-empty room (the highest-importance entry in that room).

2. **For each room**, read the representative, then pull the rest:
   ```bash
   ndx recall drawer list --room <name> --limit 50 --json
   ```
   Synthesize a concise summary (≤300 chars) that captures:
   - The room's theme in one sentence
   - The 2-4 most important facts in the room
   - Any outstanding questions or contradictions

3. **Create the summary drawer**
   ```bash
   ndx recall drawer add "Summary of <room>: ..." --room _summary_ --importance 10
   ```
   If a `_summary_` drawer already exists for this room (check via `ndx recall drawer list --room _summary_`), `update` it instead of creating a new one — otherwise you end up with multiple summaries per room.

4. **Link the summary to the source drawers** (optional)
   ```bash
   ndx recall drawer link --from <summary_id> --to <source_id> --kind derived_from
   ```
   This preserves the audit trail from summary back to the drawers that produced it.

## Stopping criteria

- Every active room has a single current summary drawer in `_summary_`.
- Re-run after significant mining or classification runs to refresh summaries.

## Guidelines

- Keep summaries tight. A 300-char summary is more useful in wake-up than a 1000-char essay.
- Prefer concrete statements ("uses Postgres 16 for OLTP") over vague ones ("uses a database").
- If a room has fewer than 3 drawers, it probably doesn't need a summary yet; skip it.
- The `_summary_` room is reserved. Don't put non-summary content there.
"#;

const SKILL_CHORE: &str = r#"# /ndx-chore — Palace maintenance orchestrator

Walk the four palace-hygiene phases — classify, score, dedupe, contradict — to completion in one go. This is the daily/per-session maintenance entry point. Use the focused sub-skills (`/ndx-recall-classify`, `/ndx-recall-score`, `/ndx-recall-dedupe`, `/ndx-recall-contradict`) only for surgical review.

`/ndx-chore` does **not** run summarize (run after big curation rounds) or handover (run before `/exit`). Those are different cadences.

## Phases (run in order)

For each phase, fetch the pending queue, walk it to empty, and print a one-line summary.

### Phase 1: Classify

```bash
ndx recall drawer list --pending classify --limit 25 --json
```

Returns drawers with `room == "unclassified"`. For each one:
- Decide the best-fitting room from `project.existing_rooms`. Create a new room only when none fits.
- Bulk-classify by source file first when the pattern is obvious:
  ```bash
  ndx recall drawer update --source-file CHANGELOG.md --room releases
  ```
- Otherwise: `ndx recall drawer update --id <N> --room <name>` (auto-creates the room).
- Pure noise (separators, single punctuation, boilerplate headers): `ndx recall drawer rm --id <N>`.

Floor rules:
- Room names: lowercase, `[a-z0-9_-]+`, ≤64 chars.
- If a drawer is genuinely ambiguous, leave it as `unclassified` and move on — do not invent rooms to clear the queue.
- Re-fetch the batch after bulk moves to see what remains.

Stop: `ndx recall drawer list --pending classify --limit 1 --json` returns an empty `drawers` array.

Print: `classify: N assigned, M skipped` (skipped = drawers you left ambiguous).

### Phase 2: Score

```bash
ndx recall drawer list --pending score --limit 25 --json
```

Returns drawers with `importance == 5` and `source_kind != Manual`. For each one:
- Bulk-score by source file when patterns are clear: `ndx recall drawer update --source-file CHANGELOG.md --importance 4`.
- Otherwise: `ndx recall drawer update --id <N> --importance <1..10>`.
- Aggressive noise (markdown separators, headings with no body): score 1-2 or `drawer rm`.

Floor rules (1-10 scale):
- **10** — core identity, irreversible constraints. Always wake-up loaded.
- **7-9** — important decisions, rationale, architectural pillars, regular collaborators.
- **4-6** — normal context, code patterns, general project facts.
- **1-3** — low-signal noise, incidental output, dedup-amplified boilerplate.

Be stingy with 9-10. When uncertain, pick the lower value.

Stop: pending queue empty.

Print: `score: N rated, M skipped`.

### Phase 3: Dedupe

```bash
ndx recall drawer list --pending dedupe --limit 20 --json
```

Returns clusters of drawers sharing content-hash prefixes. For each cluster:
- **True duplicates** (same claim, same angle): keep the highest-importance one, `drawer rm` the rest, optionally bump survivor's importance.
- **Complementary** (same claim, different detail): merge text into one with `drawer update --text "..."`, `drawer rm` the others.
- **Coincidental hash collision**: skip — they are not actually related.
- **Timeline** ("we use Postgres" → "we switched to Cockroach"): use `drawer link --kind supersedes` instead of merging or deleting.

Floor rules:
- Be conservative. When in doubt, leave duplicates alone — extra noise is recoverable, lost content is not.
- Prefer `supersedes` links over deletion when the older drawer has historical value.

Stop: pending queue empty.

Print: `dedupe: N merged, M superseded, K skipped`.

### Phase 4: Contradict

```bash
ndx recall drawer list --pending contradict --limit 30 --json
```

Returns drawers that already participate in some link. For each candidate pair:
- Real contradiction (incompatible claims about the same topic): `ndx recall drawer link --from <A> --to <B> --kind contradicts`.
- One side is clearly stale: use `--kind supersedes` from the correct one to the stale one (hides stale from L1).
- Different topics that share vocabulary, complementary facts, or sequential decisions: skip.

Floor rules:
- Judgment-heavy. When unsure, do nothing — false `contradicts` links add noise; missed ones are recoverable.
- If it takes a paragraph of reasoning to see the conflict, it probably is not one.
- Report unresolved contradictions to the user at the end.

Stop: pending queue empty.

Print: `contradict: N flagged, M superseded, K skipped`.

## Stopping criteria

- All four phases must walk their queue to empty (or to a steady-state where every remaining item is a deliberate skip).
- If a drawer cannot be judged confidently, **skip it and continue** — do not abort the phase.
- If all four phases start empty: print `nothing to do` and exit.
- At the end, count drawers that need user review (unresolved contradictions, ambiguous classifications you flagged) and print: `review needed: N drawers`.

## Escape hatch

When a phase produces too many ambiguous decisions or you want full rule detail with examples, drop into the focused sub-skill: `/ndx-recall-classify`, `/ndx-recall-score`, `/ndx-recall-dedupe`, `/ndx-recall-contradict`. Each one covers a single phase exhaustively.
"#;

pub fn run_install() -> Result<()> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let ndx_dir = home.join(".ndx");
    let commands_dir = ndx_dir.join("commands");
    let ndx_bin = std::env::current_exe().context("cannot determine ndx binary path")?;
    let ndx_bin_str = ndx_bin.to_string_lossy().into_owned();

    // 1. Create directories
    std::fs::create_dir_all(&commands_dir)?;
    eprintln!("Created {}", commands_dir.display());

    // 2. Download manifests
    eprintln!("Downloading command manifests from kcp-commands...");
    let manifest_count = download_manifests(&commands_dir);
    eprintln!("  Manifests: {} files in {}", manifest_count, commands_dir.display());

    // 3. Register hook in ~/.claude/settings.json (no MCP server)
    let settings_path = home.join(".claude").join("settings.json");
    register_claude_settings(&settings_path, &ndx_bin_str)?;
    eprintln!(
        "  Hook: PreToolUse (Bash), PreCompact, SessionStart, SessionEnd registered in {}",
        settings_path.display()
    );

    // 4. Install global skills (main ndx.md + recall slash commands)
    let skill_dir = home.join(".claude").join("commands");
    install_skill(&skill_dir)?;
    eprintln!(
        "  Skills: {} files installed to {}",
        SKILL_FILES.len(),
        skill_dir.display()
    );

    // 5. Remove obsolete skills left behind by older ndx versions.
    let pruned = prune_global_orphans(&skill_dir)?;
    for name in &pruned {
        eprintln!("  Removed obsolete skill: {}", name);
    }

    eprintln!();
    eprintln!("ndx install complete");
    eprintln!("  Restart Claude Code to activate.");

    Ok(())
}

/// Wire ndx into a project. Slash commands themselves are not copied
/// here — they live globally in `~/.claude/commands/` (installed by
/// `ndx install`) and are visible to every project Claude Code opens.
/// This function only touches per-project files: the `.gitignore`
/// entry for `.ndx/` and the `## ndx` section in `CLAUDE.md`.
pub fn install_skill_to_project(project_dir: &Path) -> Result<()> {
    ensure_gitignore_entry(project_dir)?;
    ensure_claude_md_ndx_section(project_dir)?;
    Ok(())
}

/// Files that ndx wrote to `<project>/.claude/commands/` in older
/// versions before slash commands were standardized as global-only.
/// `cleanup_project_skills` removes these from a project on demand.
/// User-authored files that happen to share the `ndx-` prefix are
/// untouched — only canonical names appear in this list.
pub const OBSOLETE_SKILLS: &[&str] = &["ndx-recall-refresh.md"];

/// Result of `cleanup_project_skills`: which canonical skill copies
/// were removed and which were preserved because git tracks them.
#[derive(Debug, Default)]
pub struct CleanupReport {
    pub removed: Vec<PathBuf>,
    pub needs_git_rm: Vec<PathBuf>,
    pub dir_removed: bool,
}

/// Returns true iff `path` (relative or absolute) is tracked by git
/// in the repository at `project_dir`. If `project_dir` is not a git
/// working tree, returns false (callers treat untracked-file as
/// "safe to remove").
fn is_git_tracked(project_dir: &Path, path: &Path) -> bool {
    let rel = path.strip_prefix(project_dir).unwrap_or(path);
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(project_dir)
        .arg("ls-files")
        .arg("--error-unmatch")
        .arg(rel)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Remove `<project>/.claude/commands/<canonical>` for every canonical
/// skill file (current `SKILL_FILES` plus historical `OBSOLETE_SKILLS`),
/// refusing to touch git-tracked files. Foreign files in the directory
/// are preserved. The directory itself is removed if empty afterwards.
pub fn cleanup_project_skills(project_dir: &Path) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    let cmd_dir = project_dir.join(".claude").join("commands");
    if !cmd_dir.exists() {
        return Ok(report);
    }

    let canonical: Vec<&str> = SKILL_FILES
        .iter()
        .map(|(n, _)| *n)
        .chain(OBSOLETE_SKILLS.iter().copied())
        .collect();

    for name in canonical {
        let path = cmd_dir.join(name);
        if !path.exists() {
            continue;
        }
        if is_git_tracked(project_dir, &path) {
            report.needs_git_rm.push(path);
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing {}", path.display()))?;
            report.removed.push(path);
        }
    }

    // Remove `.claude/commands/` if it's now empty. Leave `.claude/`
    // alone — other tools may use it.
    if let Ok(mut entries) = std::fs::read_dir(&cmd_dir) {
        if entries.next().is_none() {
            std::fs::remove_dir(&cmd_dir).ok();
            report.dir_removed = true;
        }
    }

    Ok(report)
}

/// Remove any obsolete canonical skills from the global directory.
/// Unconditional and idempotent — runs every time `ndx install` is
/// invoked. Returns the list of names that were actually removed.
pub fn prune_global_orphans(skill_dir: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for name in OBSOLETE_SKILLS {
        let path = skill_dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing {}", path.display()))?;
            removed.push((*name).to_string());
        }
    }
    Ok(removed)
}

const CLAUDE_MD_NDX_SECTION: &str = r#"
## ndx

`ndx` is available in this project. Use `/ndx` for full CLI reference.

Key commands: `ndx recall search "query"` (hybrid search), `ndx recall wake` (context), `ndx xref drawer <file>` (cross-ref).

Skills: `/ndx-chore` (daily maintenance orchestrator), `/ndx-recall-classify`, `/ndx-recall-score`, `/ndx-recall-dedupe`, `/ndx-recall-contradict`, `/ndx-recall-summarize`, `/ndx-recall-handover`.

If recall palace is not initialized, run `ndx recall init` then `ndx recall mine --from-memory`.
"#;

/// Append an ndx section to the project's `CLAUDE.md` if not already
/// present. Creates the file if it doesn't exist. Idempotent.
pub fn ensure_claude_md_ndx_section(project_dir: &Path) -> Result<()> {
    let claude_md = project_dir.join("CLAUDE.md");
    let marker = "## ndx";

    if claude_md.exists() {
        let content = std::fs::read_to_string(&claude_md)?;
        if content.contains(marker) {
            return Ok(()); // already present
        }
        let prefix = if content.ends_with('\n') { "" } else { "\n" };
        std::fs::write(
            &claude_md,
            format!("{}{}{}", content, prefix, CLAUDE_MD_NDX_SECTION),
        )?;
    } else {
        std::fs::write(&claude_md, CLAUDE_MD_NDX_SECTION.trim_start())?;
    }
    Ok(())
}

/// Append `.ndx/` to the project's `.gitignore` if not already present.
/// Creates the file if it doesn't exist. Idempotent.
pub fn ensure_gitignore_entry(project_dir: &Path) -> Result<()> {
    let gitignore = project_dir.join(".gitignore");
    let entry = ".ndx/";

    if gitignore.exists() {
        let content = std::fs::read_to_string(&gitignore)?;
        if content.lines().any(|l| l.trim() == entry) {
            return Ok(()); // already present
        }
        // Append with a preceding newline if the file doesn't end with one.
        let prefix = if content.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&gitignore, format!("{}{}{}\n", content, prefix, entry))?;
    } else {
        std::fs::write(&gitignore, format!("{}\n", entry))?;
    }
    Ok(())
}

/// The complete set of skill files shipped by `ndx install` and
/// `ndx init`. The main `ndx.md` documents the general CLI surface;
/// the five `ndx-recall-*.md` files are specialized slash commands for
/// recall palace maintenance.
pub const SKILL_FILES: &[(&str, &str)] = &[
    ("ndx.md", SKILL_CONTENT),
    ("ndx-chore.md", SKILL_CHORE),
    ("ndx-recall-classify.md", SKILL_RECALL_CLASSIFY),
    ("ndx-recall-score.md", SKILL_RECALL_SCORE),
    ("ndx-recall-dedupe.md", SKILL_RECALL_DEDUPE),
    ("ndx-recall-contradict.md", SKILL_RECALL_CONTRADICT),
    ("ndx-recall-summarize.md", SKILL_RECALL_SUMMARIZE),
    ("ndx-recall-handover.md", SKILL_RECALL_HANDOVER),
];

fn install_skill(skill_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(skill_dir)?;
    for (name, content) in SKILL_FILES {
        let path = skill_dir.join(name);
        std::fs::write(&path, content)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

fn download_manifests(commands_dir: &PathBuf) -> usize {
    // Fetch index.txt with ureq
    let index_body = match ureq::get(MANIFEST_INDEX_URL).call() {
        Ok(resp) => match resp.into_string() {
            Ok(body) => body,
            Err(_) => {
                eprintln!("  Warning: could not read manifest index body");
                return 0;
            }
        },
        Err(e) => {
            eprintln!("  Warning: could not fetch manifest index: {}", e);
            eprintln!("    URL: {}", MANIFEST_INDEX_URL);
            return 0;
        }
    };

    let keys: Vec<String> = index_body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();

    let total = keys.len();
    let downloaded = AtomicUsize::new(0);
    let progress = AtomicUsize::new(0);

    // Parallel download with rayon
    keys.par_iter().for_each(|key| {
        let url = format!("{}/{}.yaml", MANIFEST_BASE_URL, key);
        let dest = commands_dir.join(format!("{}.yaml", key));

        if let Ok(resp) = ureq::get(&url).call() {
            if let Ok(body) = resp.into_string() {
                if std::fs::write(&dest, body).is_ok() {
                    downloaded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let done = progress.fetch_add(1, Ordering::Relaxed) + 1;
        if done % 50 == 0 || done == total {
            eprint!("\r  Downloading: {}/{}", done, total);
        }
    });
    eprintln!();

    downloaded.load(Ordering::Relaxed)
}

/// Idempotently register an ndx-owned hook entry for a `hook_event_name`
/// that takes no matcher (PreCompact, SessionStart, SessionEnd). Strips
/// any existing ndx-owned entry first so a re-install picks up a new
/// `ndx_bin` path or status message.
fn register_event_hook(
    hooks_obj: &mut serde_json::Map<String, serde_json::Value>,
    event_name: &str,
    ndx_bin: &str,
    status_message: &str,
) {
    let entry = hooks_obj
        .entry(event_name.to_string())
        .or_insert_with(|| serde_json::json!([]));

    if let Some(arr) = entry.as_array_mut() {
        arr.retain(|entry| {
            if let Some(hooks_arr) = entry.get("hooks").and_then(|v| v.as_array()) {
                for h in hooks_arr {
                    if let Some(cmd) = h.get("command").and_then(|v| v.as_str()) {
                        if cmd.contains("ndx") && cmd.contains("hook") {
                            return false;
                        }
                    }
                }
            }
            true
        });

        arr.push(serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": format!("{} hook", ndx_bin),
                "timeout": 10,
                "statusMessage": status_message,
            }]
        }));
    }
}

fn register_claude_settings(settings_path: &PathBuf, ndx_bin: &str) -> Result<()> {
    // Ensure directory exists
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Load existing settings or create new
    let mut settings: serde_json::Value = if settings_path.exists() {
        let data = std::fs::read_to_string(settings_path)?;
        serde_json::from_str(&data).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let obj = settings.as_object_mut().context("settings must be an object")?;

    // Remove ndx MCP server if previously registered
    if let Some(mcp_servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        mcp_servers.remove("ndx");
        // Remove mcpServers key entirely if empty
        if mcp_servers.is_empty() {
            obj.remove("mcpServers");
        }
    }

    // Register PreToolUse + PreCompact hooks
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(hooks_obj) = hooks.as_object_mut() {
        // ── PreToolUse (Bash) ─────────────────────────────────────────
        let pre_tool_use = hooks_obj
            .entry("PreToolUse")
            .or_insert_with(|| serde_json::json!([]));

        if let Some(arr) = pre_tool_use.as_array_mut() {
            // Remove existing kcp-commands or ndx entries
            arr.retain(|entry| {
                let matcher = entry.get("matcher").and_then(|v| v.as_str());
                if matcher != Some("Bash") {
                    return true;
                }
                // Check if it's a kcp or ndx hook
                if let Some(hooks_arr) = entry.get("hooks").and_then(|v| v.as_array()) {
                    for h in hooks_arr {
                        if let Some(cmd) = h.get("command").and_then(|v| v.as_str()) {
                            if cmd.contains("kcp") || cmd.contains("ndx") {
                                return false;
                            }
                        }
                    }
                }
                true
            });

            // Add ndx hook
            arr.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": format!("{} hook", ndx_bin),
                    "timeout": 10,
                    "statusMessage": "ndx: looking up command manifest..."
                }]
            }));
        }

        // ── PreCompact (no matcher → fires for manual + auto) ─────────
        register_event_hook(
            hooks_obj,
            "PreCompact",
            ndx_bin,
            "ndx: re-injecting recall palace wake-up...",
        );

        // ── SessionStart (no matcher → fires on startup/resume/clear/compact) ─
        register_event_hook(
            hooks_obj,
            "SessionStart",
            ndx_bin,
            "ndx: auto-mining recent sessions...",
        );

        // ── SessionEnd (observational, no additionalContext) ──────────
        register_event_hook(
            hooks_obj,
            "SessionEnd",
            ndx_bin,
            "ndx: mining ended session...",
        );
    }

    let output = serde_json::to_string_pretty(&settings)?;
    std::fs::write(settings_path, output)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Running `register_claude_settings` twice must not duplicate
    /// either the PreToolUse or the PreCompact entry. Both must point
    /// at the supplied `ndx_bin` path.
    #[test]
    fn register_claude_settings_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let ndx_bin = "/tmp/fake/ndx";

        register_claude_settings(&settings_path, ndx_bin).unwrap();
        register_claude_settings(&settings_path, ndx_bin).unwrap();

        let body = std::fs::read_to_string(&settings_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();

        let pre_tool = v
            .pointer("/hooks/PreToolUse")
            .and_then(|x| x.as_array())
            .expect("PreToolUse array");
        let ndx_pre_tool: Vec<_> = pre_tool
            .iter()
            .filter(|e| {
                e.get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|arr| {
                        arr.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("ndx"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            ndx_pre_tool.len(),
            1,
            "PreToolUse should have exactly one ndx entry after two installs"
        );

        let pre_compact = v
            .pointer("/hooks/PreCompact")
            .and_then(|x| x.as_array())
            .expect("PreCompact array");
        let ndx_pre_compact: Vec<_> = pre_compact
            .iter()
            .filter(|e| {
                e.get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|arr| {
                        arr.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("ndx"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            ndx_pre_compact.len(),
            1,
            "PreCompact should have exactly one ndx entry after two installs"
        );

        // Updating the bin path rewrites both entries, still idempotent.
        let new_bin = "/usr/local/bin/ndx";
        register_claude_settings(&settings_path, new_bin).unwrap();
        let body2 = std::fs::read_to_string(&settings_path).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&body2).unwrap();
        let pre_compact2 = v2
            .pointer("/hooks/PreCompact")
            .and_then(|x| x.as_array())
            .unwrap();
        let hits: Vec<_> = pre_compact2
            .iter()
            .filter(|e| {
                e.get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|arr| {
                        arr.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains(new_bin))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(hits.len(), 1, "PreCompact must track the new bin path");
    }

    /// Two installs must leave exactly one SessionStart and one
    /// SessionEnd entry.
    #[test]
    fn install_registers_session_hooks_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let ndx_bin = "/tmp/fake/ndx";

        register_claude_settings(&settings_path, ndx_bin).unwrap();
        register_claude_settings(&settings_path, ndx_bin).unwrap();

        let body = std::fs::read_to_string(&settings_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();

        for event in ["SessionStart", "SessionEnd"] {
            let arr = v
                .pointer(&format!("/hooks/{}", event))
                .and_then(|x| x.as_array())
                .unwrap_or_else(|| panic!("{} array missing", event));
            let ndx_entries: Vec<_> = arr
                .iter()
                .filter(|e| {
                    e.get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|inner| {
                            inner.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(|c| c.contains("ndx"))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
                .collect();
            assert_eq!(
                ndx_entries.len(),
                1,
                "{} should have exactly one ndx entry after two installs",
                event
            );
            // No `matcher` key — these events do not use matchers.
            for e in &ndx_entries {
                assert!(
                    e.get("matcher").is_none(),
                    "{} entries must not include a matcher key: {:?}",
                    event,
                    e
                );
            }
        }
    }

    // ── Project-side install: skills no longer copied ─────────────────

    /// `install_skill_to_project` must NOT create
    /// `<project>/.claude/commands/`. It only touches CLAUDE.md and
    /// .gitignore.
    #[test]
    fn init_does_not_create_skill_files() {
        let tmp = tempfile::tempdir().unwrap();
        install_skill_to_project(tmp.path()).unwrap();

        let cmd_dir = tmp.path().join(".claude").join("commands");
        assert!(
            !cmd_dir.exists(),
            ".claude/commands/ should not be created by ndx init"
        );

        let claude_md = tmp.path().join("CLAUDE.md");
        assert!(claude_md.exists(), "CLAUDE.md must be created");
        let body = std::fs::read_to_string(&claude_md).unwrap();
        assert!(body.contains("## ndx"));

        let gi = tmp.path().join(".gitignore");
        assert!(gi.exists(), ".gitignore must be created");
        let gibody = std::fs::read_to_string(&gi).unwrap();
        assert!(gibody.lines().any(|l| l.trim() == ".ndx/"));
    }

    // ── Project-side cleanup ──────────────────────────────────────────

    fn write_dummy(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// `cleanup_project_skills` removes canonical names but preserves
    /// foreign files. Directory survives because of the foreign file.
    #[test]
    fn cleanup_removes_canonical_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = tmp.path().join(".claude").join("commands");
        write_dummy(&cmd.join("ndx.md"), "stale");
        write_dummy(&cmd.join("ndx-chore.md"), "stale");
        write_dummy(&cmd.join("foo.md"), "user file");

        let report = cleanup_project_skills(tmp.path()).unwrap();

        assert_eq!(report.removed.len(), 2);
        assert!(report.needs_git_rm.is_empty());
        assert!(!report.dir_removed);
        assert!(!cmd.join("ndx.md").exists());
        assert!(!cmd.join("ndx-chore.md").exists());
        assert!(cmd.join("foo.md").exists());
    }

    /// Cleanup removes the directory when nothing remains after pruning
    /// canonical files.
    #[test]
    fn cleanup_removes_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = tmp.path().join(".claude").join("commands");
        write_dummy(&cmd.join("ndx.md"), "stale");
        write_dummy(&cmd.join("ndx-recall-handover.md"), "stale");

        let report = cleanup_project_skills(tmp.path()).unwrap();

        assert!(report.dir_removed, "empty .claude/commands/ should be removed");
        assert!(!cmd.exists(), "directory must be gone");
        // .claude/ itself should remain.
        assert!(tmp.path().join(".claude").exists());
    }

    /// Missing `.claude/commands/` is a no-op.
    #[test]
    fn cleanup_handles_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let report = cleanup_project_skills(tmp.path()).unwrap();
        assert!(report.removed.is_empty());
        assert!(report.needs_git_rm.is_empty());
        assert!(!report.dir_removed);
    }

    /// Cleanup also targets the historical orphan `ndx-recall-refresh.md`
    /// even though it's not in the current `SKILL_FILES`.
    #[test]
    fn cleanup_removes_obsolete_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = tmp.path().join(".claude").join("commands");
        write_dummy(&cmd.join("ndx-recall-refresh.md"), "stale");

        let report = cleanup_project_skills(tmp.path()).unwrap();

        assert_eq!(report.removed.len(), 1);
        assert!(!cmd.join("ndx-recall-refresh.md").exists());
    }

    /// Git-tracked canonical files are preserved and reported as
    /// `needs_git_rm`. Skipped if `git` is not on PATH.
    #[test]
    fn cleanup_refuses_git_tracked() {
        if std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("git not available — skipping");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();

        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        };

        run_git(&["init", "-q"]);
        run_git(&["config", "user.email", "t@t"]);
        run_git(&["config", "user.name", "t"]);

        let cmd = path.join(".claude").join("commands");
        write_dummy(&cmd.join("ndx.md"), "tracked");
        write_dummy(&cmd.join("ndx-chore.md"), "untracked");
        run_git(&["add", ".claude/commands/ndx.md"]);
        run_git(&["commit", "-q", "-m", "init"]);

        let report = cleanup_project_skills(path).unwrap();

        assert_eq!(
            report.removed.len(),
            1,
            "untracked file should have been removed"
        );
        assert!(!cmd.join("ndx-chore.md").exists());
        assert_eq!(
            report.needs_git_rm.len(),
            1,
            "tracked file should be flagged"
        );
        assert!(cmd.join("ndx.md").exists());
        assert!(report.needs_git_rm[0].ends_with("ndx.md"));
    }

    // ── Global-side orphan janitor ────────────────────────────────────

    #[test]
    fn prune_orphans_removes_recall_refresh() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ndx-recall-refresh.md"), "stale").unwrap();

        let removed = prune_global_orphans(tmp.path()).unwrap();

        assert_eq!(removed, vec!["ndx-recall-refresh.md".to_string()]);
        assert!(!tmp.path().join("ndx-recall-refresh.md").exists());
    }

    #[test]
    fn prune_orphans_preserves_user_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("my-custom.md"), "mine").unwrap();
        std::fs::write(tmp.path().join("ndx-recall-handover.md"), "current").unwrap();

        let removed = prune_global_orphans(tmp.path()).unwrap();

        assert!(removed.is_empty());
        assert!(tmp.path().join("my-custom.md").exists());
        assert!(tmp.path().join("ndx-recall-handover.md").exists());
    }

    #[test]
    fn prune_orphans_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("ndx-recall-refresh.md"), "stale").unwrap();

        let first = prune_global_orphans(tmp.path()).unwrap();
        let second = prune_global_orphans(tmp.path()).unwrap();

        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
    }
}
