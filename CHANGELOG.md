# Changelog

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
