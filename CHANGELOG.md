# Changelog

## v0.8.2 — Unreleased

- **Do-Not-Repeat L0 channel.** Reserved `_do_not_repeat_` room (analogous to the existing `_summary_`); drawers placed there render above L1 in every wake-up regardless of importance. Soft cap from `[wakeup] dnr_cap` in `identity.toml` (default 20); when exceeded, the wake-up text appends `_(N more rules in _do_not_repeat_; run /ndx-chore to consolidate)_`. DnR drawers are excluded from L1 so they don't render twice. `ndx recall status` shows the active rule count (`Do-Not-Repeat: N rules` plain, `do_not_repeat_count` JSON). The `/ndx-chore` and `/ndx-recall-classify` skill texts now call out `_do_not_repeat_` as a real classification target with the corrections/hard-rules use case.
- **Repeated-read detection.** New PreToolUse hook on `Read` captures the file's `mtime` and counts past Read events for the same `(session, path, mtime)`. When the upcoming read would be the third of identical content, emits `additionalContext`: `ndx: this session has read <path> N times — work from existing context instead of re-reading`. Bash, Grep, and external edits all bump or differ from the recorded `mtime` and naturally reset the count — no false positives on edit-then-verify cycles. `ndx install` now registers two PreToolUse matchers (Bash + Read); idempotent across re-installs. `EventEntry` gains an optional `meta` field (backward-compatible via serde default) used to store the per-Read mtime.
- **Token estimates in the file index.** `ndx list` and `ndx find` now accept `--tokens` (appends a `tokens=<N>` column) and `--json` (structured output: `path`, `size`, `modified`, `tokens`). Estimate is `size_bytes / ratio_for_extension` from a per-extension table — code (`.rs`, `.py`, `.go`, …) at 3.0, prose (`.md`, `.txt`) at 3.8, whitespace-heavy (`.json`, `.yaml`, `.toml`, …) at 4.5, default 3.5. No new index data: ratio is a pure function of extension, file size already cached. Lets Claude pick the cheapest file when several would do.
- **Slash commands now live globally only.** `ndx init` no longer copies skill files into `<project>/.claude/commands/`; it only appends the `## ndx` section to `CLAUDE.md` and adds `.ndx/` to `.gitignore`. The eight canonical skills (`ndx.md`, `ndx-chore.md`, six `ndx-recall-*`) continue to be written to `~/.claude/commands/` by `ndx install`. Slash commands are pure CLI documentation — one source of truth eliminates per-project drift.
- **`ndx init --clean-up`** removes pre-existing copies of the canonical skill set (plus the historical orphan `ndx-recall-refresh.md`) from `<project>/.claude/commands/`. Files tracked by git are preserved with an instruction to run `git rm <paths>` manually. Non-canonical files (e.g. user-authored slash commands sharing the `ndx-` prefix) are not touched. The `.claude/commands/` directory is removed when empty.
- **`ndx install` now prunes obsolete global skills.** `ndx-recall-refresh.md` is removed unconditionally if present in `~/.claude/commands/`. Idempotent — silent when already absent.

## v0.8.1 — 2026-04-29

- New `/ndx-chore` orchestrator skill that walks the four palace-hygiene phases — classify, score, dedupe, contradict — to completion in one go, with concise per-phase judgment floors and a single review-needed total. The `/ndx` skill has been rewritten around lifecycle (daily/session-end/occasional/surgical) instead of a CLI-reference dump; the full reference is preserved under a `Reference` heading.
- SessionEnd hook auto-mines the just-ended session into the palace (`mine --from-memory` scoped to the ending `session_id`, no embed). Observational — emits no `additionalContext`. Soft-fails when there is no palace rooted at `cwd`. Idempotent via the existing `MINED_SESSIONS` table.
- SessionStart hook auto-mines pending sessions on launch (no embed) and emits an `additionalContext` nudge — `# ndx-recall — palace hygiene pending` — when the sum of pending classify + score + dedupe + contradict drawers reaches the threshold (default 20). Below threshold the hook is silent. Mirrors PreCompact's `hookSpecificOutput` shape. `ndx install` now registers four hooks: PreToolUse (Bash), PreCompact, SessionStart, SessionEnd.

## v0.8.0 — 2026-04-23

- **Shared palaces via symlink** — multiple checkouts of the same repository can delegate their palace to a canonical checkout. New CLI commands: `ndx recall init --link <canonical-root>`, `ndx recall link-palace <canonical-root> [--force]`, `ndx recall unlink-palace [--keep]`, `ndx recall rehome <new-canonical-root>`. `--keep` makes an MVCC point-in-time copy of the canonical palace (via a read-txn table walk) so concurrent writers on the source don't block or corrupt the copy. Symlink chains are collapsed at link time — every linked checkout points at the canonical directly. `link-palace` refuses if the canonical target is missing, and refuses (without `--force`) when the local palace already contains drawers.
- **Palace schema v3** — new `canonical_root` META entry (absolute path, UTF-8) stamped at init. `ndx recall rebuild-index` now performs both BM25 rebuild and v2→v3 migration (stamps `canonical_root`, rewrites every drawer's `source_file` to project-relative form where applicable) and is idempotent across v1/v2/v3 palaces. Strict opens of a v2 palace return a schema-version error pointing at `rebuild-index`.
- **Canonical source_file paths** — `drawer add --source-file`, mining, and bulk updates now normalize `source_file` against `canonical_root` at insert time: paths inside the root become project-relative, paths outside stay absolute, relative paths pass through. `ndx xref drawer <path>` resolves absolute-or-cwd-relative inputs against `canonical_root` before lookup, so shared palaces hit from any checkout. `recall status` surfaces `Canonical root:` and `Linked to:` (human) / `canonical_root`, `palace_linked_to` (JSON).

## v0.7.0 — 2026-04-20

- **BM25 lexical search** — the L3 lexical channel now scores candidates with Okapi BM25 over a tokenizer (lowercase, split on non-alphanumeric, drop tokens shorter than 2 chars, drop a 31-word English stopword list). Replaces the previous drawer-text trigram hit-count ranker. Parameters `k1 = 1.2`, `b = 0.75` — scientific default and the Anthropic contextual retrieval baseline. Hybrid search (semantic + lexical via RRF) now reliably beats lexical-only on synonym and rare-term queries.
- **Palace schema v2** — `drawer_trigrams` and `drawers_by_trigram` dropped. New tables `bm25_postings`, `drawers_by_token`, `drawer_lengths`, `bm25_meta`. Daemon code-index trigrams (`src/trigram.rs`, `src/memory/mod.rs`) are unaffected — they stay the right tool for code substring search.
- **`ndx recall rebuild-index`** — re-tokenise every drawer into the BM25 index without touching embeddings, then stamp the palace as schema v2. Required once after upgrading from 0.6.x. Strict opens of a v1 palace return a schema-version error pointing here; `rebuild-index` itself opens in migration mode so the upgrade always succeeds in one step. Drawers, embeddings, and links are untouched; the orphaned v1 trigram tables remain on disk (a few KB, harmless).
- **PreCompact hook** — `ndx install` now registers a second Claude Code hook that re-injects the L0+L1 recall palace wake-up text before context compaction (manual `/compact` or automatic at the context limit). Palace context survives compaction intact. Soft-fails when no palace is rooted at the session's `cwd`. The per-session `WAKE_INJECTED` gate used by the PreToolUse Bash hook is intentionally ignored here — PreCompact always re-injects. Re-running `ndx install` is idempotent: one PreCompact entry per ndx binary, and updating the binary path rewrites the entry in place.

## v0.6.3 — Bulk search-based drawer update

- `ndx recall drawer update --search <regex> --room <room>` — bulk-move drawers matching a case-insensitive regex pattern
- `--from-room <room>` — restrict search to drawers currently in a specific room
- `--dry-run` — preview matched drawers without modifying (shows first 5 with snippets)
- `--importance N` — optionally set importance alongside the room move
- Supports full regex syntax (`ring buffer|replication`, `R-10[0-9]`, etc.)

## v0.6.2 — Mining performance overhaul

- **No-embed default** — mine commands skip embedding by default; 5-10x faster. Run `ndx recall reembed` to backfill when ready to search. Use `--embed` flag to embed during mine.
- **Streaming pipeline** — mine processes drawers in chunks of 1000 instead of collecting all in memory. Prevents OOM on large projects.
- **Session tracking** — `mine --from-memory` records which sessions were mined. Re-runs skip unchanged sessions (0.4s re-mine vs 3:42 first run on a 9K-drawer project). Use `--force` to re-process all.
- **Signal filter** — `mine --from-memory` auto-filters assistant narration noise ("Let me read...", "Now I'll check...") and trivial user turns ("ok", "yes"). Keeps only decision/rationale/outcome content.
- **Batch trigram aggregation** — trigram posting-list updates aggregated per transaction batch instead of per-drawer. ~41% wall-time improvement on large mines.
- **Progress counter** — stderr shows `mining: N drawers from M sessions...` during long runs
- **Source-aware auto-rooms** — `mine --project` maps CHANGELOG.md→releases, CLAUDE.md→conventions, docs/specs/→architecture, proposals/→proposals, etc.
- **Benchmarked on 800K LoC project:** 9,121 drawers from 102 sessions in 3:42 (no embed), re-mine in 0.4s

## v0.6.1 — Fix UTF-8 boundary panics

- Fixed 7 potential panics when truncating or slicing strings at non-ASCII character boundaries
- Added `safe_truncate` and `safe_prefix` helpers (zero-cost for ASCII input)
- Affects: drawer text truncation at 8KiB limit, session-id display, command event logging

## v0.6.0 — Auto-discovery, prebuilt binaries, CLAUDE.md integration

### Added
- `ndx init` now appends an ndx section to the project's `CLAUDE.md` — Claude automatically discovers ndx, its skills, and key commands without manual configuration
- `install.sh` downloads prebuilt binaries from GitHub Releases (macOS ARM64/x86_64, Linux x86_64/aarch64), falls back to source build if unavailable
- "Recall Palace Workflow" section in README with step-by-step lifecycle guide
- `/ndx-recall-handover` skill — session knowledge handover to save durable insights as memories

### Changed
- `install.sh` rewritten: prebuilt-first instead of source-only
- README installation section updated with prebuilt download path
- 7 skill files now ship with `ndx install` / `ndx init`

## v0.5.3 — Workflow docs, curl installer, handover skill

- Added "Recall Palace Workflow" section to README with step-by-step lifecycle guide
- Added `install.sh` — one-line from-source installer (`curl | bash`)
- Added `/ndx-recall-handover` skill — session knowledge handover
- Updated Installation section with curl install + manual paths
- 7 skill files now ship with `ndx install` / `ndx init`

## v0.5.2 — /ndx-recall-handover skill

- Added `/ndx-recall-handover` slash command

## v0.5.1 — QoL: auto-gitignore .ndx/

- `ndx init` and `ndx recall init` now automatically add `.ndx/` to the project's `.gitignore` (creates the file if absent, idempotent on re-run)

## v0.5.0 — Recall: Structured Episodic Memory Palace

Released 2026-04-09. Design spec: [`docs/specs/recall.md`](docs/specs/recall.md).

### Added

#### Recall palace (`ndx recall`)
- **Per-project structured memory** at `{project}/.ndx/recall.redb` with drawers (atomic memory units), rooms (topic buckets), and typed links (`references`, `contradicts`, `supersedes`, `derived_from`)
- **BLAKE3 content-hash dedup** — repeat ingests bump importance on the existing drawer instead of creating duplicates
- **4-layer retrieval ladder:**
  - **L0** — identity text rendered from global `~/.ndx/identity.toml` deep-merged with optional per-project override
  - **L1** — importance-ranked, room-grouped wake-up text (top 15 drawers, ≤3200 chars, excludes `Supersedes` targets)
  - **L2** — room-filtered retrieval via `ndx recall get --room <name>`
  - **L3** — hybrid search: fastembed `all-MiniLM-L6-v2` cosine (K_sem=50) fused with trigram intersection (K_lex=50) via Reciprocal Rank Fusion (k=60)
- **Three mining modes:** `mine --from-memory` (turn-pair drawers from global session history filtered by current project), `mine --from-chroma <path>` (direct sqlite read of a mempalace ChromaDB export), `mine --project [--path]` (walks the project tree and paragraph-chunks text files)
- **Full drawer CRUD** — `drawer add|list|show|update|rm|link|unlink` with cascade on delete across all satellite tables
- **Claude-curated maintenance via slash commands** — `/ndx-recall-classify`, `/ndx-recall-score`, `/ndx-recall-dedupe`, `/ndx-recall-contradict`, `/ndx-recall-summarize` delegate judgment to Claude Code and round-trip through `ndx recall drawer` CLI
- **Pending-op read schema** — `drawer list --pending <op> --json` emits `{op, project, drawers, cursor}` for skill batch consumption
- **Structured JSON write-back envelopes** — all `--json` write commands emit `{"ok": bool, ...}` on stdout; JSON error envelopes emit `{"ok": false, "error": ..., "code": N}` with the correct exit code

#### Cross-references
- `ndx xref drawer <file>` — resolves file paths to drawers via direct `source_file` match plus trigram-narrowed basename substring confirm
- `ndx xref drawer-session <session-id>` — drawers derived from a session
- `ndx xref git <commit>` — walks `git diff-tree` for the commit, unions drawer sets across changed files, caches the result (passive, no git hooks installed)

#### Hook wake-up injection
- PreToolUse Bash hook injects L0+L1 wake-up text exactly once per Claude session, wrapped in a `# ndx-recall wake-up (session ...)` marker block
- Soft-fails at every decision point (no session id, no cwd, no palace, embedder error) so existing manifest-hint behavior is unaffected
- `ndx recall wake --force` clears all session markers so the next Bash hook re-injects (picks up `identity.toml` edits, for example)

#### Skills and installation
- `ndx install` now drops 6 skill files (main `ndx.md` + 5 recall slash commands) to `~/.claude/commands/`
- `ndx init <path>` drops the same 6 files to `<path>/.claude/commands/`
- Main `ndx.md` rewritten with full recall surface documentation

### Dependencies
- Added `fastembed = "4"` for local MiniLM-L6-v2 embeddings (pulls `ort` / onnxruntime)
- Added `toml = "0.8"` for identity file parsing
- Added `rusqlite = "0.32"` (bundled) for direct mempalace ChromaDB read
- Added `blake3 = "1"` for content hashing

### Deliberate non-goals
- No AAAK compression layer (mempalace's lossy dialect; benchmark regression)
- No MCP server (ndx is pure CLI, subagent-friendly by construction)
- No ChromaDB runtime dependency (only read-only import)
- No global cross-project palace in v1 (identity is global, drawers are strictly per-project)
- No pre-commit git hooks (cross-ref is on-demand only)
- No heuristic NLP in Rust (classification/scoring/dedup/contradict/summarize all delegate to Claude via skills)

### Migration notes
This is a pure addition: all existing file-index, memory, xref, and hook behaviors are preserved. The file-index daemon and `~/.ndx/memory.redb` are unchanged. Recall palaces are opt-in via `ndx recall init`.

## v0.4.0 — Parallel Indexing & Incremental Scanning

### Added
- **Incremental scanning** — tracks mtime/size per file in FILE_HASHES table; only changed files are re-indexed on daemon restart
- **Parallel trigram extraction** — rayon-based parallel content indexing (3-5x speedup on large projects)
- **Watcher debouncing** — 200ms debounce window batches filesystem events into bulk transactions
- **Parallel manifest downloads** — ureq + rayon replaces 289 sequential curl subprocesses (~20x faster)
- **Doc ID persistence** — next_doc_id restored from META table across daemon restarts (no ID collisions)
- **Batch file removal** — `remove_files_batch()` handles deletions in a single transaction
- **Pre-computed trigram indexing** — `index_content_batch_precomputed()` accepts pre-extracted trigram maps for parallel pipelines

### Changed
- **Streaming trigram extraction** — replaced `HashSet<u32>` per trigram with `Vec<u32>` + last-element dedup (halves memory for large files)
- **Scanner no longer calls `index.clear()`** — incremental diff instead of full rebuild
- **Watcher processes events in batches** — 3 transactions per batch instead of N per-file transactions

### Dependencies
- Added `rayon = "1"` for data parallelism
- Added `ureq = "2"` for HTTP (replaces curl subprocesses)

## v0.3.0 — CLI + Daemon Architecture

**Breaking change:** ndx is no longer an MCP server. It is now a pure CLI tool backed by a background daemon.

### Added
- **Background daemon** — auto-starts on first index query, owns the project index exclusively, keeps it current via filesystem watcher
- **Unix domain socket IPC** — CLI commands communicate with daemon via `.ndx/ndx.sock`
- **CLI subcommands** — `search`, `list`, `find`, `status`, `memory`, `xref`, `ping`, `stop`
- **Auto-start** — daemon spawns automatically on first CLI query; no manual setup needed
- **Stale socket detection** — client detects dead daemons, cleans up, and restarts
- **Daemon logging** — stderr output goes to `.ndx/ndx.log`
- **Claude Code skill** — `ndx init` installs a skill file to `.claude/commands/ndx.md`
- **Global skill install** — `ndx install` registers the skill in `~/.claude/commands/`

### Changed
- **No MCP dependency** — removed `rmcp` and `schemars` crates
- **Subagent-friendly** — all functionality accessible via Bash, works from subagents and team members
- **Memory commands use direct access** — no daemon needed for session/event/agent queries
- **`ndx init`** — now installs skill file instead of creating `.mcp.json`
- **`ndx install`** — registers hook + skill (no longer registers MCP server); cleans up old MCP entries

### Removed
- MCP server mode (`ndx [path]` no longer starts an MCP server)
- `rmcp`, `schemars` dependencies

## v0.2.0 — Episodic Memory, Command Hooks, Cross-Referencing

### Added
- Episodic memory — indexes Claude Code session transcripts for full-text search
- Command hooks — PreToolUse syntax injection and output filtering via YAML manifests
- Cross-referencing — file-to-session and session-to-file queries
- Subagent transcript parsing and search
- Event logging (Phase C) for command history
- `ndx install` — downloads 289 command manifests from kcp-commands
- `ndx scan` — explicit memory scanning
- Context lines, output modes, pagination, glob filter for content search
- ISO 8601 timestamp formatting

## v0.1.0 — Initial Release

### Added
- File metadata index with prefix and glob filtering
- Trigram content search with line-level positions
- Filesystem watcher for real-time index updates
- Gitignore-aware scanning
- MCP server on stdio
- redb embedded database
