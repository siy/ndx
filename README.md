# ndx

Fast file index with trigram search, real-time file watching, episodic memory, and command hooks. Single Rust binary, zero external dependencies. Runs as a background daemon with a thin CLI client.

## Features

- **Fast file listing** — indexed file tree with prefix and glob filtering
- **Trigram content search** — line-level positions for literal queries, regex with trigram-accelerated candidate narrowing
- **Real-time updates** — background daemon with filesystem watcher re-indexes on create/modify/delete
- **Gitignore-aware** — respects `.gitignore` rules, rebuilds matcher on changes
- **Episodic memory** — indexes AI coding session transcripts for full-text search across past sessions
- **Command hooks** — PreToolUse hook injects CLI syntax hints and filters noisy output
- **Cross-referencing** — bridges file index and session memory ("which sessions touched this file?")
- **Subagent-friendly** — pure CLI interface works from any context, including Claude Code subagents and team members
- **Zero configuration** — single binary, no JVM, no Node.js, no external services

## Architecture

ndx uses a daemon+client architecture:

```
CLI (ndx search/list/find/status)
  └─ client ──UDS──► daemon (background process)
                       ├─ index.redb (exclusive owner)
                       ├─ scanner (initial full scan)
                       ├─ watcher (real-time updates)
                       └─ query engine (trigram search)

CLI (ndx memory/xref)
  └─ direct access ──► memory.redb
```

The daemon auto-starts on the first index query and stays alive until `ndx stop`. It owns the project index exclusively, keeps it current via filesystem watcher, and serves queries over a Unix domain socket (`.ndx/ndx.sock`).

Memory commands access the global memory database directly — no daemon needed.

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

After building, run the installer to download command manifests and register the hook and skill:

```sh
ndx install
```

This will:
1. Download 289 YAML command manifests from [kcp-commands](https://github.com/Cantara/kcp-commands) to `~/.ndx/commands/`
2. Register the PreToolUse Bash hook in `~/.claude/settings.json`
3. Install the ndx skill to `~/.claude/commands/ndx.md`

### Per-project setup

```sh
cd /path/to/your/project
ndx init
```

Installs the ndx skill into the project's `.claude/commands/` directory.

## CLI

### File index commands

All file index commands communicate with the background daemon (auto-started on first use).

```sh
ndx search <pattern>                    # trigram-accelerated content search
ndx search "TODO" --file-pattern "*.rs" # filter files by glob
ndx search "fn main" -B 2 -A 5         # context lines before/after
ndx search "error" --output files       # output mode: content, files, count
ndx search "pattern" --offset 100       # pagination

ndx list                                # list all indexed files
ndx list --path src/ --pattern "*.rs"   # filter by prefix and glob
ndx list --sort modified               # sort by modification time

ndx find "**/*.toml"                    # find files matching glob
ndx find "src/**/*.rs" --sort modified

ndx status                              # index + memory statistics
```

### Memory commands

Direct access to the global memory database — no daemon needed.

```sh
ndx memory search "database migration"          # search session transcripts
ndx memory events "docker"                       # search command event log
ndx memory list                                  # recent sessions
ndx memory list --project /path/to/project       # filter by project
ndx memory stats                                 # session/event/agent counts
ndx memory session <session-id>                  # full session details
ndx memory context                               # recent project context
ndx memory subagents "search query"              # search subagent transcripts
ndx memory tree <session-id>                     # session + subagent tree
```

All memory commands accept `--limit N`.

### Cross-reference commands

```sh
ndx xref file src/main.rs               # find sessions that touched this file
ndx xref session <session-id>           # list files touched by a session
```

### Daemon commands

```sh
ndx ping                                # check if daemon is running
ndx stop                                # stop the background daemon
```

### Maintenance commands

```sh
ndx scan                                # scan memory (sessions, events, agents)
ndx install                             # download manifests, register hook + skill
ndx init [path]                         # install ndx skill into a project
```

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

1. On first query, the daemon starts and walks the project tree, building a file metadata index plus trigram content index in [redb](https://github.com/cberner/redb)
2. A filesystem watcher keeps the file index current in real-time
3. CLI commands connect to the daemon via Unix domain socket for index queries
4. The global memory database at `~/.ndx/memory.redb` indexes session transcripts from `~/.claude/projects/`
5. Content search extracts trigrams from queries to narrow candidates before confirming matches
6. The hook subcommand resolves manifests and responds in <20ms

## Data Storage

| Database | Location | Contents |
|----------|----------|----------|
| Project index | `{project}/.ndx/index.redb` | File metadata, trigram content index |
| Daemon socket | `{project}/.ndx/ndx.sock` | Unix domain socket for client-daemon IPC |
| Daemon log | `{project}/.ndx/ndx.log` | Daemon stderr output |
| Global memory | `~/.ndx/memory.redb` | Sessions, events, agents, cross-references |
| Manifests | `~/.ndx/commands/*.yaml` | Command syntax and filter definitions |

## Environment Variables

- `RUST_LOG` — control log verbosity (e.g. `RUST_LOG=ndx=debug`). Daemon logs go to `.ndx/ndx.log`.

## Acknowledgments

ndx's episodic memory and command manifest features are inspired by and compatible with:

- **[kcp-commands](https://github.com/Cantara/kcp-commands)** — Command syntax injection and output filtering for AI coding agents. Created by [Cantara](https://github.com/Cantara). ndx uses the same YAML manifest format and downloads the same bundled manifests (289 commands covering git, docker, kubectl, cloud CLIs, build tools, and more).

- **[kcp-memory](https://github.com/Cantara/kcp-memory)** — Episodic memory daemon for AI coding sessions. Created by [Cantara](https://github.com/Cantara). ndx implements equivalent session transcript parsing and CLI interfaces in Rust.

- **[Knowledge Context Protocol](https://github.com/Cantara/knowledge-context-protocol)** — The KCP specification that defines the manifest format and integration patterns.

Both kcp-commands and kcp-memory are licensed under [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0). ndx is an independent Rust implementation of the same concepts and protocols. The YAML manifest files downloaded by `ndx install` are redistributed from kcp-commands under their original Apache 2.0 license.
