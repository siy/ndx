# ndx v0.2.0 Design Specification

**Status:** Draft
**Date:** 2026-03-15
**Scope:** Episodic memory, command manifest hooks, cross-referencing, installation

---

## Table of Contents

1. [Overview](#1-overview)
2. [Architecture](#2-architecture)
3. [Data Model — redb Table Schemas](#3-data-model--redb-table-schemas)
4. [Feature 1: Episodic Memory](#4-feature-1-episodic-memory)
5. [Feature 2: Command Manifest Hook](#5-feature-2-command-manifest-hook)
6. [Feature 3: Cross-Referencing](#6-feature-3-cross-referencing)
7. [Feature 4: Installation](#7-feature-4-installation)
8. [CLI Interface](#8-cli-interface)
9. [MCP Tool Schemas](#9-mcp-tool-schemas)
10. [Module Organization](#10-module-organization)
11. [Error Handling Strategy](#11-error-handling-strategy)
12. [Performance Requirements](#12-performance-requirements)
13. [Migration & Backward Compatibility](#13-migration--backward-compatibility)
14. [Feature 5: Documentation & Attribution](#14-feature-5-documentation--attribution)
15. [Open Questions](#15-open-questions)
16. [References](#16-references)

---

## 1. Overview

ndx v0.2.0 extends the existing Rust MCP file index server with three major capabilities:

1. **Episodic memory** — indexes Claude Code session transcripts and tool-call events, making past sessions searchable via MCP tools. Replaces kcp-memory (Java/SQLite) with a native Rust implementation using redb and trigram search.

2. **Command manifest hook** — acts as a Claude Code PreToolUse hook for Bash commands, injecting syntax context before execution and filtering noisy output after. Replaces kcp-commands (Java daemon + Node.js) with a single Rust binary. Logs events directly into the redb index (no intermediate JSONL file).

3. **Cross-referencing** — bridges file indexing and session memory so users can ask "which sessions touched this file?" and "which files did this session touch?"

### Design Goals

- **Single binary.** No JVM, no Node.js, no external daemons. One `ndx` binary serves MCP, runs hooks, and manages installation.
- **Single database.** All data (file metadata, trigram index, sessions, events, manifests) lives in one redb database at `{project_root}/.ndx/index.redb` for project-scoped data and `~/.ndx/memory.redb` for global memory data.
- **Backward compatible.** All four existing MCP tools (`list_files`, `search_files`, `search_content`, `index_status`) continue working without changes.
- **Fast hooks.** The `ndx hook` subcommand must respond in <20ms to avoid perceptible delay on every Bash call.

### What Is NOT In Scope

- Gemini CLI / Codex CLI transcript parsing (Claude Code only for v0.2.0)
- Auto-generating manifests from `--help` output (deferred to v0.3.0)
- HTTP daemon API (ndx uses stdio MCP and direct CLI, not HTTP)
- PostToolUse hook for output filtering (Phase B filtering is done via `updatedInput` command wrapping in the PreToolUse hook itself)

---

## 2. Architecture

```
                                ┌──────────────────────────────────────┐
                                │           ndx binary                 │
                                │                                      │
  Claude Code                   │  ┌─────────┐    ┌─────────────────┐ │
  ───stdin/stdout JSON-RPC────▶ │  │  MCP     │    │  Memory         │ │
                                │  │  Server  │───▶│  (redb global)  │ │
                                │  │  (rmcp)  │    │  sessions,      │ │
                                │  │          │    │  events, agents │ │
                                │  │  14 tools│    └─────────────────┘ │
                                │  └─────────┘                         │
                                │       │         ┌─────────────────┐  │
                                │       └────────▶│  File Index     │  │
                                │                 │  (redb project) │  │
                                │                 │  files, trigrams │  │
  Claude Code PreToolUse        │                 └─────────────────┘  │
  ───stdin JSON → stdout JSON──▶│  ┌─────────┐                        │
                                │  │  Hook    │    ┌─────────────────┐ │
                                │  │  Engine  │───▶│  Manifests      │ │
                                │  │  (<20ms) │    │  (YAML on disk) │ │
                                │  └─────────┘    └─────────────────┘  │
                                │       │                              │
                                │       └──────▶ writes events ──────▶ │
                                │                Memory (redb global)  │
                                └──────────────────────────────────────┘

  Filesystem watcher ──────▶ File Index (project .ndx/index.redb)
  Session scanner ─────────▶ Memory     (global  ~/.ndx/memory.redb)
```

### Two Database Strategy

| Database | Location | Contents | Lifecycle |
|----------|----------|----------|-----------|
| **Project index** | `{project_root}/.ndx/index.redb` | File metadata, trigram content index | Created per project, rebuilt on startup |
| **Global memory** | `~/.ndx/memory.redb` | Sessions, events, agents, event cursor | Created once, never rebuilt, append-only |

The project index is the existing v0.1.0 database, unchanged. The global memory database is new in v0.2.0 and stores data that spans all projects.

---

## 3. Data Model — redb Table Schemas

### 3.1 Existing Tables (project index, unchanged)

| Table | Key Type | Value Type | Purpose |
|-------|----------|------------|---------|
| `files` | `&str` (rel path) | `&[u8]` (JSON `FileEntry`) | File metadata |
| `trigrams` | `&[u8]` (3-byte trigram) | `&[u8]` (packed posting entries) | Content index posting lists |
| `doc_paths` | `u32` (doc ID) | `&str` (rel path) | Reverse lookup doc_id → path |
| `path_ids` | `&str` (rel path) | `u32` (doc ID) | Forward lookup path → doc_id |

### 3.2 New Tables (global memory database)

#### `sessions` — Indexed session transcripts

```
TableDefinition<&str, &[u8]>  // key: session_id, value: JSON-encoded SessionEntry
Table name: "sessions"
```

```rust
#[derive(Serialize, Deserialize)]
pub struct SessionEntry {
    pub session_id: String,
    pub project_dir: String,
    pub git_branch: Option<String>,
    pub slug: String,               // from ~/.claude/projects/<slug>/
    pub model: Option<String>,
    pub started_at: Option<String>,  // ISO-8601
    pub ended_at: Option<String>,    // ISO-8601
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tool_names: Vec<String>,     // distinct tool names used
    pub files: Vec<String>,          // file paths touched (from tool_input)
    pub first_message: Option<String>, // first human turn, <=500 chars
    pub all_user_text: String,       // concatenated human turns, <=8000 chars
    pub scanned_at: String,          // ISO-8601, when ndx last processed this file
    pub source_path: String,         // absolute path to the .jsonl file
    pub source_modified: u64,        // mtime epoch secs of source file at scan time
}
```

#### `sessions_by_project` — Secondary index for project-filtered queries

```
TableDefinition<&str, &[u8]>  // key: "{project_dir}\0{started_at}\0{session_id}", value: b""
Table name: "sessions_by_project"
```

Key is a composite string enabling range scans by project_dir prefix, ordered by started_at descending within each project.

#### `session_trigrams` — Trigram index for session full-text search

```
TableDefinition<&[u8], &[u8]>  // key: 3-byte trigram, value: packed posting entries (doc_id, line_num=0)
Table name: "session_trigrams"
```

Reuses the same trigram engine as file content search. The "document" being indexed is the concatenation of `first_message` + `all_user_text` for each session. `doc_id` maps to `session_doc_paths`/`session_path_ids`.

#### `session_doc_paths` / `session_path_ids` — Doc ID mapping for session trigrams

```
TableDefinition<u32, &str>    // session_doc_paths: doc_id → session_id
TableDefinition<&str, u32>    // session_path_ids: session_id → doc_id
Table name: "session_doc_paths", "session_path_ids"
```

#### `events` — Tool-call events

```
TableDefinition<u64, &[u8]>   // key: auto-increment ID, value: JSON-encoded EventEntry
Table name: "events"
```

```rust
#[derive(Serialize, Deserialize)]
pub struct EventEntry {
    pub event_ts: String,        // ISO-8601
    pub session_id: String,
    pub project_dir: String,
    pub tool: String,            // "Bash" typically
    pub command: String,         // first 500 chars
    pub manifest_key: Option<String>,
    pub ingested_at: String,     // ISO-8601
}
```

#### `event_trigrams` / `event_doc_paths` / `event_path_ids` — Trigram index for events

Same structure as session trigrams. The "document" is `command` + `project_dir` concatenated. Enables full-text search over events.

```
Table names: "event_trigrams", "event_doc_paths", "event_path_ids"
```

#### `events_by_project` — Secondary index for project-filtered event queries

```
TableDefinition<&str, u64>    // key: "{project_dir}\0{event_ts}", value: event ID
Table name: "events_by_project"
```

#### `event_cursor` — Byte offset tracking for incremental event ingestion

```
TableDefinition<u8, u64>      // key: 0 (singleton), value: byte offset
Table name: "event_cursor"
```

#### `events_dedup` — Deduplication index for events

```
TableDefinition<&str, ()>     // key: "{event_ts}\0{session_id}\0{command_hash}", value: ()
Table name: "events_dedup"
```

Uses a blake3 hash of the command string (truncated to 16 bytes hex) to keep key size bounded.

#### `agents` — Subagent session transcripts

```
TableDefinition<&str, &[u8]>  // key: agent_id, value: JSON-encoded AgentEntry
Table name: "agents"
```

```rust
#[derive(Serialize, Deserialize)]
pub struct AgentEntry {
    pub agent_id: String,            // e.g. "ab5a2b081b1711482"
    pub parent_session_id: String,
    pub agent_slug: Option<String>,
    pub project_dir: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub turn_count: u32,
    pub tool_call_count: u32,
    pub tool_names: Vec<String>,
    pub first_message: Option<String>,
    pub all_user_text: String,
    pub first_seen_at: Option<String>,
    pub last_updated_at: Option<String>,
    pub message_count: u32,
    pub scanned_at: String,
    pub source_path: String,
    pub source_modified: u64,
}
```

#### `agents_by_parent` — Secondary index for parent session lookup

```
TableDefinition<&str, &str>   // key: "{parent_session_id}\0{agent_id}", value: ""
Table name: "agents_by_parent"
```

#### `agent_trigrams` / `agent_doc_paths` / `agent_path_ids` — Trigram index for agents

Same pattern as session and event trigrams. Indexes `first_message` + `all_user_text`.

#### `session_files_xref` — Cross-reference: file path → session IDs

```
TableDefinition<&str, &[u8]>  // key: file_path, value: JSON array of session_ids
Table name: "session_files_xref"
```

Built during session scanning. When a session's `files` list contains a path, that path is added to this cross-reference.

#### `next_doc_ids` — Per-namespace doc ID counters

```
TableDefinition<&str, u32>    // key: namespace ("session", "event", "agent"), value: next doc_id
Table name: "next_doc_ids"
```

Persisted instead of using AtomicU32 (since the memory database is not rebuilt on startup).

---

## 4. Feature 1: Episodic Memory

### 4.1 Session Transcript Scanning

**Source:** `~/.claude/projects/**/*.jsonl`

**Scanning algorithm:**

1. Walk `~/.claude/projects/` recursively, collecting all `.jsonl` files
2. Skip files in `subagents/` directories (handled separately in 4.3)
3. For each file, check `source_modified` in the existing `sessions` entry — skip if file has not changed since `scanned_at`
4. Parse the JSONL file line by line (same logic as kcp-memory's `SessionParser.parseClaude`)
5. Extract: `session_id` (from `sessionId` field), `project_dir` (from `cwd`), `git_branch`, `model`, timestamps, turn count, tool calls, files touched, user text
6. Upsert into `sessions` table
7. Update `sessions_by_project` secondary index
8. Update `session_trigrams` index (trigram-index the `first_message` + `all_user_text`)
9. Update `session_files_xref` for each file path in the session's files list

**JSONL line format (Claude Code transcript):**

```jsonc
// Human turn:
{"type":"human","message":{"content":[{"type":"text","text":"..."}]},"timestamp":"...","sessionId":"...","cwd":"..."}

// Assistant turn:
{"type":"assistant","message":{"model":"claude-opus-4-6","content":[
  {"type":"text","text":"..."},
  {"type":"tool_use","name":"Read","input":{"file_path":"/some/file"}}
]},"timestamp":"..."}
```

**Fields extracted per JSONL line:**

| Field | Source | Notes |
|-------|--------|-------|
| `session_id` | `sessionId` field on any line | First non-blank value wins |
| `project_dir` | `cwd` field on human turns | First non-blank value wins; fallback: decode slug from parent dir name |
| `git_branch` | `gitBranch` field on any line | First non-blank value wins |
| `model` | `message.model` on assistant turns | First non-blank value wins |
| `started_at` | `timestamp` of first line | ISO-8601 |
| `ended_at` | `timestamp` of last line | ISO-8601 |
| `turn_count` | Count of assistant turns | Incremented for each `type: "assistant"` line |
| `tool_call_count` | Count of `tool_use` blocks | Within assistant turn content arrays |
| `tool_names` | Distinct `name` values from `tool_use` blocks | Deduped, insertion-ordered |
| `files` | `file_path` and `path` from `tool_use.input` | Deduped set; extracted recursively from tool input JSON |
| `first_message` | First human turn text | Truncated to 500 chars |
| `all_user_text` | All human turn text concatenated | Truncated to 8000 chars |

**Slug decoding:** Claude Code stores sessions under `~/.claude/projects/<slug>/` where `<slug>` is the project directory with `/` replaced by `-`. To recover the project directory: replace `-` with `/`.

### 4.2 Event Log Ingestion

**Source:** `~/.kcp/events.jsonl` (written by kcp-commands Phase C, or by ndx hook Phase C directly)

**Ingestion algorithm (incremental, byte-offset cursor):**

1. Read `event_cursor` value from memory database (default: 0)
2. Open `~/.kcp/events.jsonl`, seek to byte offset
3. Read lines from that position
4. For each line, parse JSON:
   ```json
   {"ts":"2026-03-03T16:04:24Z","session_id":"ad732c58-...","project_dir":"/src/myproject","tool":"Bash","command":"cat /tmp/daemon.log","manifest_key":"cat"}
   ```
5. Check `events_dedup` — skip if `"{ts}\0{session_id}\0{command_hash}"` already exists
6. Insert into `events` table (auto-increment key)
7. Update `events_by_project` secondary index
8. Update `event_trigrams` index (trigram-index `command` + `project_dir`)
9. After all lines processed, update `event_cursor` to current file position

**Cursor semantics:** The cursor tracks the byte offset, not line count. This matches kcp-memory's `RandomAccessFile.seek()` approach. If the file is truncated (smaller than cursor), reset cursor to 0 and re-ingest.

**Event source:** In v0.2.0, events come from two sources:
1. Legacy: `~/.kcp/events.jsonl` written by kcp-commands (for users migrating from kcp-commands)
2. Direct: The `ndx hook` subcommand writes events directly to the memory database (no intermediate file). This is preferred.

When ndx is the hook handler, it writes events directly to `events` table. Ingestion of `~/.kcp/events.jsonl` is additive — it handles the case where kcp-commands is still installed alongside ndx.

### 4.3 Subagent Transcript Scanning

**Source:** `~/.claude/projects/**/<session-uuid>/subagents/agent-*.jsonl`

**Algorithm:**

1. During the session scan walk, collect files matching `subagents/agent-*.jsonl`
2. Derive `parent_session_id` from the parent directory name (which is the session UUID directory)
3. Derive `agent_id` from the filename: strip `agent-` prefix and `.jsonl` suffix
4. Parse JSONL with the same Claude transcript parser
5. Upsert into `agents` table
6. Update `agents_by_parent` secondary index
7. Update `agent_trigrams` index

### 4.4 Scan Triggers

| Trigger | When | Scope |
|---------|------|-------|
| MCP server startup | Once, before accepting requests | Full incremental scan of sessions, events, agents |
| `ndx scan` CLI subcommand | On demand | Full incremental scan |
| Before `memory_events_search` tool call | Inline | Events only (fast — reads only new bytes) |
| Before `memory_subagent_search` tool call | Inline | Agents only |
| Background timer | Every 30 minutes while MCP server is running | Full incremental scan |

### 4.5 Trigram Search Over Sessions

The existing trigram engine (`trigram.rs`) is reused for session search. The search flow:

1. User calls `memory_search` with query "OAuth implementation"
2. Extract trigrams from query → intersect posting lists from `session_trigrams`
3. Get candidate `session_id` values from `session_doc_paths`
4. For each candidate, load `SessionEntry` from `sessions` table
5. Verify match by checking if `all_user_text` or `first_message` contains the query (same grep-confirm pattern as file content search)
6. Return results sorted by `started_at` descending (most recent first)

---

## 5. Feature 2: Command Manifest Hook

### 5.1 Hook Protocol

**Registration in `~/.claude/settings.json`:**

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "/path/to/ndx hook",
            "timeout": 10,
            "statusMessage": "ndx: looking up command manifest..."
          }
        ]
      }
    ]
  }
}
```

**Input JSON (received on stdin):**

```json
{
  "session_id": "abc123-...",
  "transcript_path": "/Users/.../.claude/projects/.../abc123.jsonl",
  "cwd": "/home/user/my-project",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": {
    "command": "mvn test -pl core"
  },
  "tool_use_id": "toolu_01ABC123..."
}
```

**Output JSON (written to stdout):**

On match (manifest found):
```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "additionalContext": "[ndx] mvn: Apache Maven build tool\nUsage: mvn [options] [<goal(s)>]\nKey flags:\n  test: Compile and run tests  → Verify the build\n  -pl <module>: Build specific module\nPrefer:\n  mvn test  # Run all tests\n  mvn clean package -DskipTests  # Fast build",
    "updatedInput": {
      "command": "mvn test -pl core | /path/to/ndx filter mvn"
    }
  }
}
```

On no match (no manifest, or not a Bash tool call):
- Exit code 0, empty stdout (or no JSON output)
- Claude Code proceeds normally

On error:
- Exit code 0, empty stdout (fail open — never block the tool call)

### 5.2 Command Parsing

Translate the shell command string into a `(cmd, subcommand, key)` tuple for manifest lookup.

**Algorithm (matches kcp-commands `CommandParser.java`):**

```rust
pub struct ParsedCommand {
    pub key: String,        // e.g. "git-log", "ps", "mvn"
    pub cmd: String,        // e.g. "git", "ps", "mvn"
    pub subcommand: Option<String>, // e.g. Some("log"), None
}

pub fn parse_command(shell_command: &str) -> Option<ParsedCommand> {
    // 1. Skip subshells and backtick expressions
    if shell_command.contains("$(") || shell_command.contains("`") {
        return None;
    }

    // 2. Take first pipeline segment
    let first_segment = shell_command.split(&['|', '&', ';'][..])
        .next()?.trim();

    // 3. Strip leading env var assignments and sudo
    //    Regex: ^(?:\w+=\S+\s+)*(?:sudo\s+)?
    let stripped = strip_env_and_sudo(first_segment);

    // 4. Split on whitespace
    let parts: Vec<&str> = stripped.split_whitespace().collect();
    if parts.is_empty() { return None; }

    let cmd = parts[0].to_string();

    // 5. Compound key: if second arg looks like a subcommand word
    //    Pattern: ^[a-z][a-z0-9-]*$
    if parts.len() > 1 && is_subcommand_word(parts[1]) {
        let sub = parts[1].to_string();
        return Some(ParsedCommand {
            key: format!("{}-{}", cmd, sub),
            cmd,
            subcommand: Some(sub),
        });
    }

    Some(ParsedCommand {
        key: cmd.clone(),
        cmd,
        subcommand: None,
    })
}
```

**Subcommand detection pattern:** `^[a-z][a-z0-9-]*$` — matches lowercase words like `log`, `diff`, `get`, `ps`, `images`. Rejects flags (`--flag`), paths (`/usr/bin`), filenames (`foo.yaml`).

**Filterability check:** A command is filterable (safe to pipe through `ndx filter`) if it does not contain `>`, `<`, or `exec`.

### 5.3 Manifest Lookup Chain

Three-tier resolution, first match wins:

| Priority | Path | Scope | Notes |
|----------|------|-------|-------|
| 1 | `.kcp/commands/{key}.yaml` | Project-local | Checked-in overrides |
| 2 | `~/.kcp/commands/{key}.yaml` | User-level | User customizations and auto-generated |
| 3 | `~/.ndx/commands/{key}.yaml` | Bundled | Downloaded by `ndx install` |

The lookup uses the compound key first (e.g. `git-log`), then falls back to the simple key (e.g. `git`) if no compound manifest exists.

**Performance:** Manifests are cached in a `HashMap<String, CommandManifest>` at process startup from the bundled directory (`~/.ndx/commands/`). Project-local and user-level manifests are read from disk on each lookup (to pick up edits without restart). Since the hook process is short-lived (one invocation per Bash call), "startup" means the `ndx hook` process start, and caching applies within that single invocation.

[ASSUMPTION] For the short-lived `ndx hook` process, cold reads from disk are acceptable. If benchmarking shows >20ms, we will add a memory-mapped cache or a background daemon mode in a future version.

### 5.4 Manifest YAML Schema

```yaml
# Required fields
command: mvn                          # executable name
platform: all                         # "all" | "linux" | "macos" | "windows"
description: "Apache Maven build tool"  # one-line summary

# Optional: subcommand (for compound commands like git-log)
subcommand: null                      # or "log", "diff", etc.

# Optional: marks as auto-generated
generated: false

# Phase A: Syntax injection (optional)
syntax:
  usage: "mvn [options] [<goal(s)>]"   # usage line
  key_flags:                           # up to 5 shown
    - flag: "test"
      description: "Compile and run tests"
      use_when: "Verify the build"     # optional context hint
    - flag: "-pl <module>"
      description: "Build specific module"
      use_when: null
  preferred_invocations:               # up to 3 shown
    - invocation: "mvn test"
      use_when: "Run all tests"
    - invocation: "mvn clean package -DskipTests"
      use_when: "Fast build"

# Phase B: Output filtering (optional)
output_schema:
  enable_filter: true                  # false = skip Phase B entirely
  noise_patterns:                      # regexes to strip matching lines
    - pattern: "^\\[INFO\\] Scanning for projects"
      reason: "Boilerplate startup"    # optional documentation
    - pattern: "^\\[INFO\\] -+$"
      reason: "Separator lines"
  max_lines: 80                        # 0 = unlimited
  truncation_message: "... {remaining} more Maven lines. Check for BUILD SUCCESS/FAILURE."
```

**Rust data model:**

```rust
#[derive(Deserialize)]
pub struct CommandManifest {
    pub command: String,
    pub subcommand: Option<String>,
    pub platform: String,
    pub description: String,
    pub generated: Option<bool>,
    pub syntax: Option<SyntaxBlock>,
    pub output_schema: Option<OutputSchema>,
}

#[derive(Deserialize)]
pub struct SyntaxBlock {
    pub usage: Option<String>,
    pub key_flags: Vec<KeyFlag>,
    pub preferred_invocations: Vec<PreferredInvocation>,
}

#[derive(Deserialize)]
pub struct KeyFlag {
    pub flag: String,
    pub description: String,
    pub use_when: Option<String>,
}

#[derive(Deserialize)]
pub struct PreferredInvocation {
    pub invocation: String,
    pub use_when: String,
}

#[derive(Deserialize)]
pub struct OutputSchema {
    pub enable_filter: bool,
    pub noise_patterns: Vec<NoisePattern>,
    pub max_lines: u32,
    pub truncation_message: Option<String>,
}

#[derive(Deserialize)]
pub struct NoisePattern {
    pub pattern: String,
    pub reason: Option<String>,
}
```

### 5.5 Phase A: Syntax Context Injection

Build the `additionalContext` string from the manifest:

```
[ndx] {command} {subcommand}: {description}
Usage: {usage}
Key flags:
  {flag}: {description}  → {use_when}
  ...                                    (up to 5)
Prefer:
  {invocation}  # {use_when}
  ...                                    (up to 3)
```

If the manifest was auto-generated, append: `(auto-generated manifest -- improve it at ~/.kcp/commands/)`

### 5.6 Phase B: Output Noise Filtering

When `output_schema.enable_filter` is true and the command is filterable (no `>`, `<`, `exec`):

1. Wrap the original command via `updatedInput`:
   ```
   {original_command} | /path/to/ndx filter {manifest_key}
   ```
2. The `ndx filter` subcommand:
   - Reads stdin (raw command output)
   - Loads the manifest for the given key
   - Strips blank lines
   - For each line, test against `noise_patterns` regexes — if any matches, remove the line
   - If remaining lines exceed `max_lines`, truncate and append `truncation_message` with `{remaining}` replaced
   - Writes filtered output to stdout

**`ndx filter` CLI:**

```
ndx filter <key>
```

Reads stdin, applies the manifest's noise filter, writes to stdout. Must be fast (<5ms for typical output).

### 5.7 Phase C: Event Logging

On every `ndx hook` invocation (regardless of manifest match):

1. Extract from hook input: `session_id`, `cwd` (as `project_dir`), `tool_input.command`
2. Derive `manifest_key` from manifest resolution (or `None`)
3. Write an `EventEntry` directly to the memory database `events` table
4. This is synchronous but fast (single redb write, <1ms)

**Difference from kcp-commands:** kcp-commands writes to `~/.kcp/events.jsonl` as an intermediate file, then kcp-memory ingests it. ndx writes directly to redb, eliminating the intermediate file for ndx-originated events.

**Compatibility:** ndx still ingests `~/.kcp/events.jsonl` for events written by kcp-commands (if installed alongside ndx). Deduplication via `events_dedup` prevents double-counting.

### 5.8 Platform Filtering

Manifests with `platform: "linux"` are only loaded on Linux, `platform: "macos"` only on macOS, etc. `platform: "all"` matches everywhere. Platform is checked at manifest load time.

Current platform is determined by `std::env::consts::OS` (`"macos"`, `"linux"`, `"windows"`). The manifest `platform` field uses `"macos"` or `"darwin"` interchangeably — both match on macOS.

---

## 6. Feature 3: Cross-Referencing

### 6.1 `file_sessions` Tool

Given a file path, find sessions that touched or discussed that file.

**Algorithm:**
1. Normalize the path (resolve to absolute, then check relative to known project roots)
2. Look up the path in `session_files_xref` table
3. For each session_id found, load `SessionEntry` from `sessions` table
4. Return results sorted by `started_at` descending

**Bonus:** Also search session trigrams for the filename as a text query, to find sessions that _discussed_ the file even if they didn't touch it via tool calls.

### 6.2 `session_files` Tool

Given a session ID, list files that were touched with their current file index status.

**Algorithm:**
1. Load `SessionEntry` for the session_id from `sessions` table
2. For each file path in `session.files`:
   a. Check if the file exists in the project file index (the project-scoped redb)
   b. If yes, include with status "indexed" and metadata (size, modified)
   c. If not found in index, check filesystem directly
   d. Report status: "indexed", "exists_not_indexed", "deleted"
3. Return the file list with status annotations

**Cross-database consideration:** This tool needs to query both the global memory database (for session data) and the project-scoped file index database (for file status). The MCP server holds `Arc<Index>` for the project index and `Arc<MemoryIndex>` for the global memory.

---

## 7. Feature 4: Installation

### 7.1 `ndx install` CLI Subcommand

```
ndx install
```

**Behavior (idempotent):**

1. **Create directories:**
   ```
   ~/.ndx/
   ~/.ndx/commands/
   ```

2. **Download bundled manifests:**
   - Fetch the 289 YAML command manifests from the kcp-commands GitHub repository
   - Source: `https://github.com/Cantara/kcp-commands/releases/latest/download/kcp-commands-manifests.tar.gz`
   - Fallback: fetch individual files from `https://raw.githubusercontent.com/Cantara/kcp-commands/main/commands/{key}.yaml` using an index file
   - Save to `~/.ndx/commands/`
   - On re-run: overwrite existing files (upgrade)

3. **Register MCP server in settings:**
   - Target: `~/.claude/settings.json` (global settings)
   - Add/update `mcpServers.ndx`:
     ```json
     {
       "mcpServers": {
         "ndx": {
           "command": "/absolute/path/to/ndx",
           "args": ["."]
         }
       }
     }
     ```
   - Preserve existing entries in the file

4. **Register PreToolUse hook:**
   - Target: `~/.claude/settings.json`
   - Add/update `hooks.PreToolUse` entry:
     ```json
     {
       "hooks": {
         "PreToolUse": [
           {
             "matcher": "Bash",
             "hooks": [
               {
                 "type": "command",
                 "command": "/absolute/path/to/ndx hook",
                 "timeout": 10,
                 "statusMessage": "ndx: looking up command manifest..."
               }
             ]
           }
         ]
       }
     }
     ```
   - Remove any existing kcp-commands entries (upgrade path)
   - Preserve other PreToolUse hooks

5. **Symlink for compatibility (optional):**
   - If `~/.kcp/commands/` exists and `~/.ndx/commands/` has manifests, create a symlink or print a migration notice

6. **Print summary:**
   ```
   ndx install complete
     Manifests: ~/.ndx/commands/ (289 files)
     MCP:       registered in ~/.claude/settings.json
     Hook:      PreToolUse Bash hook registered

     Restart Claude Code to activate.
   ```

### 7.2 Standalone Install Script

For users who haven't built ndx yet:

```bash
curl -fsSL https://raw.githubusercontent.com/{repo}/main/bin/install.sh | bash
```

**Script behavior:**

1. Download the latest ndx release binary for the current platform
2. Place at `~/.ndx/ndx` (or `~/.local/bin/ndx`)
3. `chmod +x`
4. Run `ndx install` (which does the rest)

**Platform detection:** `uname -s` + `uname -m` → download `ndx-{os}-{arch}` from GitHub releases.

---

## 8. CLI Interface

### v0.1.0 (existing)

```
ndx [path]       Start MCP server for the given project root
ndx init [path]  Create .mcp.json in a project directory
ndx help         Show help
```

### v0.2.0 (additions)

```
ndx [path]           Start MCP server (unchanged, now includes memory tools)
ndx init [path]      Create .mcp.json (unchanged)
ndx hook             PreToolUse hook handler (reads stdin, writes stdout)
ndx filter <key>     Output noise filter (reads stdin, writes stdout)
ndx scan             Scan sessions + events + agents, print summary
ndx install          Download manifests, register MCP server + hook
ndx help             Show help (updated)
```

### Subcommand Details

#### `ndx hook`

- Reads hook JSON from stdin (single JSON object)
- Processes Phase A (syntax injection) + Phase B (command wrapping) + Phase C (event logging)
- Writes hook response JSON to stdout
- Exits with code 0 (always — fail open)
- Must complete in <20ms total

#### `ndx filter <key>`

- `key`: manifest key (e.g. `ps`, `git-log`, `mvn`)
- Reads raw command output from stdin
- Applies noise filter from manifest
- Writes filtered output to stdout
- Exits with code 0

#### `ndx scan`

- Performs a full incremental scan:
  1. Sessions from `~/.claude/projects/`
  2. Agents from `~/.claude/projects/**/subagents/`
  3. Events from `~/.kcp/events.jsonl`
- Prints summary to stderr:
  ```
  ndx scan complete
    Sessions: 12 indexed, 835 unchanged, 0 errors
    Agents:   4 indexed, 28 unchanged
    Events:   15 new (total: 1,247)
  ```

#### `ndx install`

See [Section 7.1](#71-ndx-install-cli-subcommand).

---

## 9. MCP Tool Schemas

### 9.1 Existing Tools (unchanged)

- `list_files` — List indexed files
- `search_files` — Find files by glob pattern
- `search_content` — Search file contents by text/regex
- `index_status` — Show file index statistics

### 9.2 New Memory Tools

#### `memory_search`

Search past Claude Code sessions by keyword. Uses trigram-indexed full-text search over session transcripts.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "Search terms — topic, technology, filename, or problem"
    },
    "limit": {
      "type": "integer",
      "description": "Max results (default 10)"
    }
  },
  "required": ["query"]
}
```

**Output format (plain text):**

```
3 session(s) for "OAuth implementation":

2026-03-10  /src/myapp
abc12345  turns=42  tools=87
"Implement OAuth2 login flow with PKCE for the React frontend..."

2026-03-08  /src/myapp
def67890  turns=15  tools=31
"Fix the OAuth refresh token handling..."
```

#### `memory_events_search`

Search past tool-call events — find specific commands Claude ran across all projects.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "Command or keyword (e.g. 'kubectl apply', 'docker build')"
    },
    "limit": {
      "type": "integer",
      "description": "Max results (default 20)"
    }
  },
  "required": ["query"]
}
```

**Output format:**

```
3 event(s) for "kubectl apply":

2026-03-03 14:32  /src/cantara/kcp-commands
abc12345  [kubectl-apply]
$ kubectl apply -f deploy.yaml

2026-02-28 11:17  /src/exoreaction/lib-pcb-app
def67890  [kubectl-apply]
$ kubectl apply -f k8s/production.yaml
```

**Pre-search action:** Triggers event log ingestion (reads new bytes from `~/.kcp/events.jsonl`) before searching.

#### `memory_list`

List recent sessions, optionally filtered by project directory.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "project": {
      "type": "string",
      "description": "Filter by project directory path (e.g. /src/myapp)"
    },
    "limit": {
      "type": "integer",
      "description": "Max results (default 20)"
    }
  },
  "required": []
}
```

**Output format:** Same as `memory_search` but without a query header, ordered by `started_at` descending.

#### `memory_stats`

Show aggregate statistics across all indexed sessions.

**Input schema:**

```json
{
  "type": "object",
  "properties": {},
  "required": []
}
```

**Output format:**

```
ndx memory statistics
---------------------------------
Sessions:    847
Turns:       12,431
Tool calls:  38,209
Events:      1,247
Agents:      142
Oldest:      2026-01-15T09:12:00Z
Newest:      2026-03-15T14:55:00Z

Top tools:
  Read                      14,821
  Bash                       9,442
  Edit                       7,103
```

#### `memory_session_detail`

Full content of a specific session by session ID.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "session_id": {
      "type": "string",
      "description": "Session ID from a memory_search or memory_list result"
    }
  },
  "required": ["session_id"]
}
```

**Output format:**

```
Session: abc12345-a7d2-4331-8ddb-7dab21e7064c
Project: /src/myapp
Branch:  feature/oauth
Model:   claude-opus-4-6
Date:    2026-03-10
Turns:   42  Tool calls: 87

Tools used: Read, Bash, Edit, Write, Grep, Glob

Files touched (12):
  src/auth/oauth.rs
  src/auth/mod.rs
  ...

User messages:
Implement OAuth2 login flow with PKCE for the React frontend. The backend should...
```

#### `memory_project_context`

Auto-detect project from PWD, return recent sessions + events. Designed to be called at the start of a session.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "project": {
      "type": "string",
      "description": "Project directory path. Defaults to the MCP server's project root."
    }
  },
  "required": []
}
```

**Output format:** Combines last 5 sessions and last 20 events for the project, formatted like `memory_list` + `memory_events_search`.

**Project resolution:** If `project` is not provided, use `self.index.root()` (the MCP server's project root). This is better than the environment variable approach used by kcp-memory since ndx already knows its project root.

#### `memory_subagent_search`

Search within subagent transcripts from past sessions.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "query": {
      "type": "string",
      "description": "Search terms to find in subagent transcripts"
    },
    "parent_session_id": {
      "type": "string",
      "description": "Limit results to agents from a specific parent session"
    },
    "limit": {
      "type": "integer",
      "description": "Max results (default 10)"
    }
  },
  "required": ["query"]
}
```

**Output format:**

```
3 subagent session(s) for "Flyway migration":

ab5a2b08  [sub of abc12345]  /src/myapp
turns=8  tools=15  (Read, Bash, Grep)
"Investigate the Flyway migration setup and determine if V3 needs..."
```

**Pre-search action:** Triggers agent scan before searching.

#### `memory_session_tree`

Show a parent session and all its child subagents as a tree.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "session_id": {
      "type": "string",
      "description": "Parent session ID from memory_search or memory_list"
    }
  },
  "required": ["session_id"]
}
```

**Output format:**

```
Session: abc12345-a7d2-4331-8ddb-7dab21e7064c
Date:    2026-03-10
Project: /src/myapp
Turns:   42  Tool calls: 87
Task:    "Implement OAuth2 login flow with PKCE..."

Subagents (3):
  |-- ab5a2b08  [/src/myapp]
  |   turns=8  tools=15  (Read, Bash, Grep)
  |   "Investigate the Flyway migration setup..."
  |
  |-- cd9e3f12  [/src/myapp]
  |   turns=12  tools=23  (Read, Write, Edit, Bash)
  |   "Implement the OAuth token refresh handler..."
  |
  |-- ef1a2b34  [/src/myapp]
  |   turns=5  tools=8  (Read, Grep)
  |   "Check existing test coverage for auth module..."
  |
```

### 9.3 New Cross-Reference Tools

#### `file_sessions`

Given a file path, find sessions that touched or discussed it.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "path": {
      "type": "string",
      "description": "File path (absolute or relative to project root)"
    },
    "limit": {
      "type": "integer",
      "description": "Max results (default 10)"
    }
  },
  "required": ["path"]
}
```

**Output format:** Same session format as `memory_search`.

#### `session_files`

Given a session ID, list files that were touched with their current status.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "session_id": {
      "type": "string",
      "description": "Session ID"
    }
  },
  "required": ["session_id"]
}
```

**Output format:**

```
Session abc12345: 12 files touched

  src/auth/oauth.rs          indexed  4,521 bytes  2026-03-10T14:32:00Z
  src/auth/mod.rs            indexed  892 bytes    2026-03-10T14:30:00Z
  src/auth/token.rs          deleted
  tests/auth_test.rs         indexed  2,103 bytes  2026-03-10T14:35:00Z
```

---

## 10. Module Organization

### New modules (v0.2.0)

```
src/
  main.rs           # extended: new subcommands (hook, filter, scan, install)
  index.rs          # unchanged: project file index
  trigram.rs         # unchanged: trigram engine (reused by memory)
  scanner.rs         # unchanged: project file scanner
  watcher.rs         # unchanged: project file watcher
  server.rs          # extended: new MCP tools (10 new tools)

  memory/
    mod.rs           # MemoryIndex struct (wraps global redb)
    session.rs       # SessionEntry, session scanning, session search
    event.rs         # EventEntry, event ingestion, event search
    agent.rs         # AgentEntry, agent scanning, agent search
    xref.rs          # Cross-referencing logic (file_sessions, session_files)
    transcript.rs    # JSONL transcript parser (Claude Code format)

  hook/
    mod.rs           # Hook entry point (read stdin, write stdout)
    parser.rs        # Command parser (extract cmd, subcommand)
    manifest.rs      # CommandManifest struct, YAML loading, lookup chain
    context.rs       # Phase A: build additionalContext string
    filter.rs        # Phase B: noise filtering + truncation

  install.rs         # ndx install subcommand
```

### Key struct relationships

```
main.rs
  |
  +-- NdxServer { index: Arc<Index>, memory: Arc<MemoryIndex>, tool_router }
  |     |
  |     +-- Index         (project redb — existing)
  |     +-- MemoryIndex   (global redb — new)
  |
  +-- hook::handle_hook(stdin) -> stdout   (short-lived process, no server)
  +-- hook::filter(key, stdin) -> stdout   (short-lived process)
  +-- install::run()                       (one-shot)
  +-- scan::run()                          (one-shot)
```

### MemoryIndex struct

```rust
pub struct MemoryIndex {
    db: Database,
    // Next doc IDs loaded from persistent storage on open
    next_session_doc_id: AtomicU32,
    next_event_doc_id: AtomicU32,
    next_agent_doc_id: AtomicU32,
    next_event_id: AtomicU64,
}

impl MemoryIndex {
    /// Open or create the global memory database at ~/.ndx/memory.redb
    pub fn open() -> Result<Self>;

    // Session operations
    pub fn upsert_session(&self, entry: &SessionEntry) -> Result<()>;
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionEntry>>;
    pub fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<SessionEntry>>;
    pub fn list_sessions(&self, project: Option<&str>, limit: usize) -> Result<Vec<SessionEntry>>;
    pub fn session_stats(&self) -> Result<MemoryStats>;

    // Event operations
    pub fn insert_event(&self, entry: &EventEntry) -> Result<()>;
    pub fn search_events(&self, query: &str, limit: usize) -> Result<Vec<EventEntry>>;
    pub fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<EventEntry>>;
    pub fn get_event_cursor(&self) -> Result<u64>;
    pub fn set_event_cursor(&self, offset: u64) -> Result<()>;

    // Agent operations
    pub fn upsert_agent(&self, entry: &AgentEntry) -> Result<()>;
    pub fn search_agents(&self, query: &str, parent: Option<&str>, limit: usize) -> Result<Vec<AgentEntry>>;
    pub fn list_agents_by_parent(&self, parent_session_id: &str, limit: usize) -> Result<Vec<AgentEntry>>;

    // Cross-reference operations
    pub fn sessions_for_file(&self, file_path: &str, limit: usize) -> Result<Vec<SessionEntry>>;
    pub fn files_for_session(&self, session_id: &str) -> Result<Vec<String>>;
}
```

---

## 11. Error Handling Strategy

### Principle: Never Break the User's Workflow

| Component | Error Strategy | Rationale |
|-----------|---------------|-----------|
| MCP server startup | Fatal — exit with error | Server can't function without index |
| File index operations | Log + continue | Individual file errors shouldn't crash the server |
| Memory scan (sessions) | Log + skip file | Malformed transcripts shouldn't prevent other sessions from indexing |
| Memory scan (events) | Log + skip line | Malformed JSONL lines shouldn't stop event ingestion |
| Hook (Phase A) | Swallow — return empty | Hook must never block a Bash call |
| Hook (Phase B) | Swallow — pass through raw | Filter failure should show unfiltered output, not no output |
| Hook (Phase C) | Swallow — log to stderr | Event logging failure is not user-facing |
| Filter subcommand | Swallow — pass through stdin to stdout | If filter fails, the command output should still reach Claude |
| Install | Report errors, continue where possible | Partial install is better than no install |

### Error Types

```rust
/// Errors specific to memory operations. Never propagated to MCP tool responses
/// as hard failures — always converted to descriptive error text.
#[derive(thiserror::Error, Debug)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Database(#[from] redb::Error),

    #[error("scan error: {0}")]
    Scan(String),

    #[error("parse error in {path}: {reason}")]
    Parse { path: String, reason: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
```

### Hook Error Handling

The `ndx hook` process must exit 0 in all cases. Any error results in empty stdout (Claude Code proceeds as if no hook was registered).

```rust
fn main() {
    match run_hook() {
        Ok(Some(response)) => {
            // Print JSON response to stdout
            println!("{}", serde_json::to_string(&response).unwrap());
        }
        Ok(None) => {
            // No manifest match — empty output, exit 0
        }
        Err(e) => {
            // Log to stderr (visible in verbose mode), exit 0
            eprintln!("[ndx hook] error: {}", e);
        }
    }
    // Always exit 0
}
```

---

## 12. Performance Requirements

### REQ-PERF-1: Hook Latency

| Metric | Target | Rationale |
|--------|--------|-----------|
| Hook total (stdin → stdout) | <20ms p95 | Runs on every Bash call; kcp-commands Java daemon does 14ms |
| Manifest YAML parse | <2ms | Single file, serde_yaml |
| Command parse | <0.1ms | String manipulation only |
| Context build | <0.1ms | String concatenation |
| Event write to redb | <2ms | Single key-value insert |

**Optimization strategy:**
- The `ndx hook` binary starts, reads stdin, resolves manifest, writes stdout, and exits. No MCP server, no file scanning, no watcher.
- Bundled manifests are loaded from individual YAML files (not preloaded into memory — each hook invocation loads at most 2 files: compound key + fallback simple key).
- redb database open for event write uses `Database::create()` which memory-maps the file — subsequent writes within the same process are fast.

### REQ-PERF-2: Filter Latency

| Metric | Target | Rationale |
|--------|--------|-----------|
| Filter total | <5ms for typical output | Piped inline with command execution |
| Regex compilation | <1ms | Compile once per invocation |

### REQ-PERF-3: MCP Tool Response Time

| Tool | Target | Notes |
|------|--------|-------|
| `memory_search` | <50ms | Trigram index lookup + verify |
| `memory_events_search` | <100ms | Includes event ingestion scan |
| `memory_list` | <20ms | Direct range scan |
| `memory_stats` | <20ms | Aggregate scan |
| `memory_session_detail` | <10ms | Single key lookup |
| `memory_project_context` | <100ms | Includes event ingestion |
| `memory_subagent_search` | <100ms | Includes agent scan |
| `memory_session_tree` | <20ms | Prefix range scan |
| `file_sessions` | <20ms | Cross-reference lookup |
| `session_files` | <20ms | Single key + file index lookups |
| Existing tools | Unchanged | No regression |

### REQ-PERF-4: Memory Database Size

| Metric | Target | Rationale |
|--------|--------|-----------|
| Per session | ~4KB | SessionEntry JSON + trigram entries |
| Per event | ~200 bytes | EventEntry JSON + trigram entries |
| 1000 sessions + 10,000 events | <10MB | Typical active user |

### REQ-PERF-5: Scan Performance

| Metric | Target |
|--------|--------|
| Full session scan (1000 files) | <10s |
| Incremental session scan (no changes) | <1s |
| Event ingestion (1 new event) | <1ms |
| Event ingestion (1000 new events) | <500ms |

---

## 13. Migration & Backward Compatibility

### 13.1 Existing ndx Users

- All v0.1.0 CLI commands continue working unchanged
- The project-scoped `.ndx/index.redb` database format is unchanged
- New tools appear in the MCP tools list alongside existing ones
- The `index_status` tool output is unchanged

### 13.2 Existing kcp-commands Users

- ndx reads the same YAML manifest format — all 289 bundled manifests are compatible
- ndx respects the same three-tier lookup chain (`.kcp/commands/`, `~/.kcp/commands/`, bundled)
- `ndx install` removes kcp-commands hook entries from `~/.claude/settings.json` and adds ndx entries
- Users can run both kcp-commands and ndx simultaneously during migration (ndx will ingest events from `~/.kcp/events.jsonl`)

### 13.3 Existing kcp-memory Users

- ndx reads `~/.kcp/events.jsonl` (the same file kcp-memory reads)
- ndx does NOT read kcp-memory's `~/.kcp/memory.db` (SQLite) — it builds its own index from the original transcript sources
- First `ndx scan` after installation will index all existing sessions from scratch into `~/.ndx/memory.redb`
- Users should remove kcp-memory from their MCP server config after verifying ndx works

### 13.4 Database Versioning

The global memory database includes a version table:

```
TableDefinition<&str, u32>  // key: "schema_version", value: version number
Table name: "meta"
```

v0.2.0 sets `schema_version = 1`. Future versions can check this and run migrations.

---

## 14. Feature 5: Documentation & Attribution

### README.md Updates

The ndx README.md must include:

1. Updated feature list mentioning episodic memory and command hooks
2. Updated MCP tools table (all 14 tools)
3. Updated CLI usage section
4. Installation section with `ndx install`
5. Attribution section:

```markdown
## Acknowledgments

ndx v0.2.0's episodic memory and command manifest features are inspired by and compatible with:

- [kcp-commands](https://github.com/Cantara/kcp-commands) — Command syntax injection and output filtering for Claude Code. Created by [Cantara](https://github.com/Cantara). ndx uses the same YAML manifest format and ships the same 289 bundled manifests.
- [kcp-memory](https://github.com/Cantara/kcp-memory) — Episodic memory daemon for Claude Code sessions. Created by [Cantara](https://github.com/Cantara). ndx implements the same session transcript parsing and MCP tool interfaces.
- [Knowledge Context Protocol](https://github.com/Cantara/knowledge-context-protocol) — The KCP specification that defines the manifest format and integration patterns.

Both kcp-commands and kcp-memory are licensed under Apache 2.0. ndx's manifest parsing and session scanning code is an independent Rust implementation inspired by their Java source.
```

### License Compatibility

- kcp-commands: Apache 2.0
- kcp-memory: Apache 2.0
- ndx: [TBD — must be Apache 2.0 compatible]

ndx does not copy code from either project — it is an independent Rust implementation of the same concepts and protocols. The YAML manifest files are data, not code, and are redistributed under their original Apache 2.0 license.

---

## 15. Open Questions

- [TBD-1] **Manifest auto-generation:** Should v0.2.0 include auto-generating manifests from `--help` output for unknown commands? This adds complexity to the hook path. Recommendation: defer to v0.3.0.

- [TBD-2] **Background daemon mode:** If hook latency exceeds 20ms due to process startup overhead, should ndx support a warm daemon mode (like kcp-commands' Java daemon on localhost:7734)? The Rust binary startup should be fast enough (~2ms) but this needs benchmarking.

- [TBD-3] **Memory database location:** Should the global memory database be at `~/.ndx/memory.redb` or `~/.kcp/memory.redb`? Using `~/.ndx/` avoids confusion with kcp-memory's `~/.kcp/memory.db` (SQLite).

- [TBD-4] **Manifest download mechanism:** Should `ndx install` download a tarball of all manifests, fetch them individually, or embed them in the binary at compile time? Embedding is simplest (no network needed at install time) but increases binary size by ~1MB.

- [TBD-5] **Session watcher:** Should ndx watch `~/.claude/projects/` for new/modified session files (like it watches project files)? This would enable near-real-time session indexing without explicit scans. Concern: watching the user's home directory tree could be expensive.

---

## 16. References

### Regulatory & Standards

- [Model Context Protocol Specification](https://modelcontextprotocol.io/) — Protocol used for MCP server communication
- [Claude Code Hooks Reference](https://code.claude.com/docs/en/hooks) — PreToolUse/PostToolUse hook JSON protocol specification

### Technical Documentation

- [redb — Rust Embedded Database](https://github.com/cberner/redb) — Key-value storage engine used by ndx
- [rmcp — Rust MCP SDK](https://crates.io/crates/rmcp) — MCP server framework used by ndx
- [Knowledge Context Protocol](https://github.com/Cantara/knowledge-context-protocol) — KCP specification defining manifest format

### Source Projects (Inspiration)

- [kcp-commands](https://github.com/Cantara/kcp-commands) — Command manifest hooks (Java/Node.js). Apache 2.0. Source of manifest YAML format, 289 bundled manifests, three-phase hook design, and command parsing logic.
- [kcp-memory](https://github.com/Cantara/kcp-memory) — Episodic memory daemon (Java/SQLite). Apache 2.0. Source of session transcript parsing, event log ingestion, MCP tool interfaces, and three-layer memory model.
- [kcp-commands release post](https://wiki.totto.org/blog/2026/03/02/kcp-commands/) — Design rationale and benchmark methodology
- [kcp-memory release post](https://wiki.totto.org/blog/2026/03/03/kcp-memory/) — Three-layer memory model design

### Internal References

- `/Users/sergiyyevtushenko/RustProjects/ndx/src/` — ndx v0.1.0 source code
- `/Users/sergiyyevtushenko/RustProjects/ndx/CLAUDE.md` — ndx v0.1.0 architecture documentation
