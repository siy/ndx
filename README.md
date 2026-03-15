# ndx

Persistent file index server via [MCP](https://modelcontextprotocol.io/) (Model Context Protocol) with episodic memory and command hooks. Single Rust binary, zero external dependencies.

## Features

- **Fast file listing** — indexed file tree with prefix and glob filtering
- **Trigram content search** — line-level positions for literal queries, regex with trigram-accelerated candidate narrowing
- **Real-time updates** — filesystem watcher re-indexes on create/modify/delete
- **Gitignore-aware** — respects `.gitignore` rules, rebuilds matcher on changes
- **Episodic memory** — indexes AI coding session transcripts for full-text search across past sessions
- **Command hooks** — PreToolUse hook injects CLI syntax hints and filters noisy output
- **Cross-referencing** — bridges file index and session memory ("which sessions touched this file?")
- **Zero configuration** — single binary, no JVM, no Node.js, no external services

## Installation

### From source

Requires [Rust](https://rustup.rs/) 1.70+.

```sh
git clone <repo>
cd ndx
cargo build --release
cp target/release/ndx ~/.local/bin/
```

### Setup

After building, run the installer to download command manifests and register with your MCP client:

```sh
ndx install
```

This will:
1. Download 289 YAML command manifests from [kcp-commands](https://github.com/Cantara/kcp-commands) to `~/.ndx/commands/`
2. Register ndx as an MCP server in `~/.claude/settings.json`
3. Register the PreToolUse Bash hook for command syntax injection

### Per-project setup

```sh
cd /path/to/your/project
ndx init
```

Creates `.mcp.json` with the ndx server configured. MCP clients pick it up automatically.

## CLI

```
ndx [path]        Start MCP server for the given project root (default: .)
ndx init [path]   Create .mcp.json in the given directory
ndx hook          PreToolUse hook handler (reads stdin, writes stdout)
ndx filter <key>  Output noise filter (reads stdin, writes stdout)
ndx scan          Scan sessions, events, and agents
ndx install       Download manifests, register MCP server + hook
ndx help          Show help
```

## MCP Tools

### File Index Tools

| Tool | Description |
|------|-------------|
| `list_files` | List indexed files with prefix/glob filtering and sorting |
| `search_files` | Find files matching a glob pattern |
| `search_content` | Trigram-accelerated content search with context lines, output modes, pagination |
| `index_status` | Show index and memory statistics |

### Memory Tools

| Tool | Description |
|------|-------------|
| `memory_search` | Search past sessions by keyword using trigram full-text search |
| `memory_events_search` | Search tool-call events across all projects |
| `memory_list` | List recent sessions, optionally filtered by project |
| `memory_stats` | Aggregate statistics (sessions, events, agents, top tools) |
| `memory_session_detail` | Full content of a specific session |
| `memory_project_context` | Recent sessions + events for current project (start-of-session context) |
| `memory_subagent_search` | Search within subagent transcripts |
| `memory_session_tree` | Parent session + child agents as a tree |

### Cross-Reference Tools

| Tool | Description |
|------|-------------|
| `file_sessions` | Find sessions that touched or discussed a given file |
| `session_files` | List files touched by a session with current status |

## Command Hook

ndx acts as a PreToolUse hook, providing three phases for every Bash command:

**Phase A — Syntax injection:** Before execution, injects CLI syntax hints (key flags, preferred invocations) from YAML manifests into the agent's context.

**Phase B — Output filtering:** Pipes output through noise filters (regex patterns) and truncation (max lines), reducing context window usage.

**Phase C — Event logging:** Records every command directly to the memory database for cross-session search.

### Manifest format

ndx uses the same YAML manifest format as [kcp-commands](https://github.com/Cantara/kcp-commands). Manifests are resolved with a three-tier lookup:

1. `.kcp/commands/<key>.yaml` — project-local (check into repo)
2. `~/.kcp/commands/<key>.yaml` — user-level customizations
3. `~/.ndx/commands/<key>.yaml` — bundled (downloaded by `ndx install`)

## How It Works

1. On startup, ndx walks the project tree and builds a file metadata index plus trigram content index in [redb](https://github.com/cberner/redb)
2. A filesystem watcher keeps the file index current
3. The global memory database at `~/.ndx/memory.redb` indexes session transcripts from `~/.claude/projects/`
4. Content search extracts trigrams from queries to narrow candidates before confirming matches
5. The hook subcommand resolves manifests and responds in <20ms

## Data Storage

| Database | Location | Contents |
|----------|----------|----------|
| Project index | `{project}/.ndx/index.redb` | File metadata, trigram content index |
| Global memory | `~/.ndx/memory.redb` | Sessions, events, agents, cross-references |
| Manifests | `~/.ndx/commands/*.yaml` | Command syntax and filter definitions |

## Environment Variables

- `RUST_LOG` — control log verbosity (e.g. `RUST_LOG=ndx=debug`). Logs go to stderr.

## Acknowledgments

ndx v0.2.0's episodic memory and command manifest features are inspired by and compatible with:

- **[kcp-commands](https://github.com/Cantara/kcp-commands)** — Command syntax injection and output filtering for AI coding agents. Created by [Cantara](https://github.com/Cantara). ndx uses the same YAML manifest format and downloads the same bundled manifests (289 commands covering git, docker, kubectl, cloud CLIs, build tools, and more).

- **[kcp-memory](https://github.com/Cantara/kcp-memory)** — Episodic memory daemon for AI coding sessions. Created by [Cantara](https://github.com/Cantara). ndx implements equivalent session transcript parsing and MCP tool interfaces in Rust.

- **[Knowledge Context Protocol](https://github.com/Cantara/knowledge-context-protocol)** — The KCP specification that defines the manifest format and integration patterns.

Both kcp-commands and kcp-memory are licensed under [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0). ndx is an independent Rust implementation of the same concepts and protocols. The YAML manifest files downloaded by `ndx install` are redistributed from kcp-commands under their original Apache 2.0 license.
