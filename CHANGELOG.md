# Changelog

## v0.5.0 — Recall: Structured Episodic Memory (in progress)

### Added
- **`ndx recall`** — per-project structured memory palace with rooms, drawers, and cross-references
- **4-layer recall ladder** — L0 identity (TOML), L1 wake-up, L2 room-filtered retrieval, L3 hybrid search
- **Hybrid semantic + lexical search** — fastembed `all-MiniLM-L6-v2` embeddings fused with trigram results via RRF
- **Drawer mining** — `mine --from-memory` (derives drawers from global session memory), `mine --from-chroma` (imports mempalace ChromaDB), `mine --project` (scans project files)
- **Cross-references** — `xref drawer <file>`, `xref drawer-session <id>`, `xref git <commit>`
- **Claude Code classification skills** — `/ndx-recall-classify`, `/ndx-recall-score`, `/ndx-recall-dedupe`, `/ndx-recall-contradict`, `/ndx-recall-summarize` delegate judgment to Claude via CLI round-trip
- **Hook wake-up injection** — PreToolUse hook injects L0+L1 context once per Claude session

### Dependencies
- Added `fastembed` for local MiniLM-L6-v2 embeddings (via `ort` / onnxruntime)
- Added `toml` for identity file parsing
- Added `rusqlite` for direct mempalace ChromaDB import

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
