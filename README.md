# ndx

Fast file index with trigram search, real-time file watching, episodic memory, **recall palace** (structured per-project memory with hybrid semantic + lexical search), and command hooks. Single Rust binary, one optional model download. Runs as a background daemon for the file index and direct-access for memory and recall.

## Features

- **Fast file listing** — indexed file tree with prefix and glob filtering
- **Trigram content search** — line-level positions for literal queries, regex with trigram-accelerated candidate narrowing
- **Real-time updates** — background daemon with filesystem watcher re-indexes on create/modify/delete
- **Gitignore-aware** — respects `.gitignore` rules, rebuilds matcher on changes
- **Episodic memory** — indexes AI coding session transcripts for full-text search across past sessions
- **Recall palace** — per-project structured memory with rooms, drawers, 4-layer retrieval ladder (identity → wake-up → room-filtered → hybrid search), local embeddings via `fastembed` + `all-MiniLM-L6-v2`
- **Hybrid search** — semantic (cosine) + lexical (BM25) fused via RRF; wins over either alone on both exact identifiers and synonyms
- **Command hooks** — PreToolUse hook injects CLI syntax hints, filters noisy output, and auto-injects wake-up context once per Claude session; PreCompact hook re-injects wake-up before context compaction so palace context survives `/compact`
- **Cross-referencing** — bridges file index, session memory, and recall palace ("which drawers touched this file?", "which drawers came from this commit?")
- **Subagent-friendly** — pure CLI interface works from any context, including Claude Code subagents and team members
- **Claude-curated quality** — five slash commands (`/ndx-recall-classify`, `-score`, `-dedupe`, `-contradict`, `-summarize`) delegate judgment work to Claude instead of brittle heuristics

## Architecture

ndx mixes a daemon+client architecture for the hot file index with direct-access storage for memory and recall:

```
CLI (ndx search/list/find/status)
  └─ client ──UDS──► daemon (background process)
                       ├─ index.redb (exclusive owner)
                       ├─ scanner (incremental parallel scan)
                       ├─ watcher (debounced real-time updates)
                       └─ query engine (trigram search)

CLI (ndx memory/xref file/session)
  └─ direct access ──► ~/.ndx/memory.redb

CLI (ndx recall *, ndx xref drawer/drawer-session/git)
  └─ direct access ──► {project}/.ndx/recall.redb
                          ├─ drawers + BM25 index + embeddings
                          ├─ rooms + links
                          ├─ file/session/commit cross-references
                          └─ wake-up injection state
```

The daemon auto-starts on the first file-index query and stays alive until `ndx stop`. It owns the project index exclusively, keeps it current via filesystem watcher, and serves queries over a Unix domain socket (`.ndx/ndx.sock`).

Memory and recall commands access their databases directly — no daemon needed. The embedding model (`all-MiniLM-L6-v2`, ~90 MiB) downloads on first semantic operation and caches in `~/.ndx/models/`.

## Installation

### Quick install

```sh
curl -fsSL https://raw.githubusercontent.com/siy/ndx/master/install.sh | bash
```

Downloads a prebuilt binary from [GitHub Releases](https://github.com/siy/ndx/releases) (macOS ARM64/x86_64, Linux x86_64/aarch64). Falls back to building from source if no prebuilt is available. Installs to `~/.local/bin/ndx`, downloads 289 command manifests, registers the PreToolUse + PreCompact hooks, and installs 7 slash commands. Restart Claude Code after install.

### From source

Requires [Rust](https://rustup.rs/) 1.70+.

```sh
git clone https://github.com/siy/ndx.git
cd ndx
cargo build --release
cp target/release/ndx ~/.local/bin/
ndx install
```

### Per-project setup

```sh
cd /path/to/your/project
ndx init                    # adds .ndx/ to .gitignore, appends ## ndx section to CLAUDE.md
ndx recall init             # creates the recall palace (.ndx/recall.redb)
```

Slash commands (`/ndx`, `/ndx-chore`, `/ndx-recall-*`) live globally in `~/.claude/commands/` after `ndx install` and are visible from every project Claude Code opens — `ndx init` does not copy them per-project. If you have an older project with stale per-project copies (from before this changed), run `ndx init --clean-up` to remove them; git-tracked copies are preserved with an explicit `git rm` instruction.

## Recall Palace Workflow

The recall palace is a per-project structured memory that stores decisions, rationale, architecture, and context — everything that disappears when a session ends. Here's the recommended lifecycle:

### 0. Introduce ndx to Claude

After `ndx init`, Claude Code sees the `/ndx` slash command which documents the full CLI surface. This is the discovery mechanism — Claude reads it and knows what ndx can do. From that point on, Claude can proactively suggest ndx commands, use recall search to find context, and run the maintenance skills without manual prompting. The tight integration flows from this one file.

### 1. Seed the palace

```sh
# Derive drawers from past Claude Code sessions about this project
ndx recall mine --from-memory

# Optionally mine specific high-value files (not the whole tree)
ndx recall mine --project --path CHANGELOG.md
ndx recall mine --project --path docs/architecture.md
```

**Tip:** Don't mine the entire repo blindly. Code files and large doc trees produce thousands of paragraph fragments that just duplicate the content. Mine session memory for decisions/rationale, and cherry-pick the files that capture *why*, not *what*.

### 2. Classify and score (via Claude)

```
/ndx-recall-classify        # assign rooms to unclassified drawers
/ndx-recall-score           # set importance (1-10) on default-5 drawers
```

Claude reads each drawer's text, proposes a room (e.g. `architecture`, `decisions`, `people`, `tools`), and scores importance. You review. Expect to delete noise drawers (assistant narration, markdown separators) during classification.

### 3. Search and retrieve

```sh
ndx recall search "why did we choose JWT"      # hybrid semantic + lexical
ndx recall wake                                 # L0 identity + L1 top drawers (for prompt injection)
ndx recall get --room decisions --limit 10      # all decisions, ranked by importance
```

The PreToolUse hook automatically injects wake-up context (L0 + L1) on the first Bash command of each Claude session — no manual `wake` needed. The PreCompact hook re-injects the same wake-up block whenever Claude compacts its context (manual `/compact` or automatic at the context limit), so palace context survives compaction intact.

### 4. Maintain over time

```
/ndx-recall-dedupe          # merge near-duplicates after large mines
/ndx-recall-contradict      # flag stale vs current claims
/ndx-recall-summarize       # generate per-room summaries
```

### 5. Hand over to the next session

```
/ndx-recall-handover        # Claude reflects on what it learned, saves as memories
```

This closes the loop: each session leaves the next one smarter. Mining, classification, and scoring compound — the palace gets more useful the more you use it.

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

### Recall palace commands

Per-project structured memory (`{project}/.ndx/recall.redb`). Drawers are atomic memory units grouped into rooms, retrievable via a 4-layer ladder plus hybrid semantic + lexical search. Direct access, no daemon.

#### Lifecycle

```sh
ndx recall init                             # create the palace
ndx recall status [--json]                  # counts, schema, embedding model, last mine
ndx recall reembed [--force]                # backfill embeddings (downloads model if needed)
```

#### Mining — fill the palace

```sh
ndx recall mine --from-memory [--since 2026-01-01]   # derive from global session memory
ndx recall mine --from-chroma <path>                 # import from a mempalace ChromaDB
ndx recall mine --project [--path <dir>]             # walk the project, paragraph-chunk text files
```

All mine modes are idempotent via BLAKE3 content-hash dedup; re-running yields `added: 0, deduped: N`.

#### Retrieval — the 4-layer ladder

```sh
ndx recall wake [--force]                   # L0 identity + L1 top drawers → stdout (wake-up text)
ndx recall get --room <name> [--limit N]    # L2 metadata retrieval
ndx recall search "query" [flags]           # L3 hybrid (default), --semantic, --lexical
ndx recall search "query" --room decisions --limit 5
```

L3 defaults to **hybrid search**: fastembed cosine similarity (top-50) fused with Okapi BM25 (`k1 = 1.2, b = 0.75`, top-50) via Reciprocal Rank Fusion (k=60). Semantic catches synonyms; lexical catches exact identifiers. Neither alone is sufficient.

#### Drawers

```sh
ndx recall drawer list [--room X] [--limit N] [--pending <op>] [--json]
ndx recall drawer show --id N [--json]
ndx recall drawer add "text" [--room X] [--importance N] [--source-file F]
ndx recall drawer update --id N [--room X] [--importance N] [--text "..."]
ndx recall drawer rm --id N                # full cascade across all indexes
ndx recall drawer link --from A --to B --kind <references|contradicts|supersedes|derived_from>
ndx recall drawer unlink --from A --to B [--kind <kind>]
```

#### Rooms and identity

```sh
ndx recall room add <name> [--title T] [--description D]
ndx recall room list | show <name> | rename <old> <new> | rm <name>
ndx recall identity show [--merged]         # render merged global + per-project identity.toml
ndx recall identity edit [--project]        # $EDITOR on the identity file (creates template)
```

#### Claude-curated maintenance (slash commands)

The palace stores everything raw. Quality is curated via five slash commands that delegate judgment to Claude Code and round-trip through `ndx recall drawer update|link|rm --json`:

| Command | Purpose |
|---|---|
| `/ndx-recall-classify` | assign rooms to `unclassified` drawers |
| `/ndx-recall-score` | set meaningful importance on default-5 drawers |
| `/ndx-recall-dedupe` | merge near-duplicates (cluster by content-hash prefix) |
| `/ndx-recall-contradict` | flag contradictions and link via `LinkKind::Contradicts` |
| `/ndx-recall-summarize` | generate per-room summary drawers in the reserved `_summary_` room |

Each skill fetches a batch via `ndx recall drawer list --pending <op> --limit N --json`, decides what to do, and writes back with individual update commands.

### Cross-reference commands

```sh
ndx xref file src/main.rs               # find sessions that touched this file
ndx xref session <session-id>           # list files touched by a session
ndx xref drawer src/auth.rs             # find palace drawers referencing a file
ndx xref drawer-session <session-id>    # drawers derived from a session
ndx xref git <commit>                   # drawers referencing files changed in a commit (cached)
```

### Daemon commands

```sh
ndx ping                                # check if daemon is running
ndx stop                                # stop the background daemon
```

### Maintenance commands

```sh
ndx scan                                # scan memory (sessions, events, agents)
ndx install                             # download manifests, register hooks, install global skills
ndx init [path] [--clean-up]            # wire ndx into a project (CLAUDE.md, .gitignore)
                                        # --clean-up removes pre-existing project-local skill copies
```

## Command Hook

ndx registers two Claude Code hooks on `ndx install`. The PreToolUse Bash hook runs three phases for every Bash command (below); the PreCompact hook re-injects the L0+L1 recall-palace wake-up text before context compaction (manual `/compact` or automatic).

**Phase A — Syntax injection:** Before execution, injects CLI syntax hints (key flags, preferred invocations) from YAML manifests into the agent's context.

**Phase B — Output filtering:** Pipes output through noise filters (regex patterns) and truncation (max lines), reducing context window usage.

**Phase C — Event logging:** Records every command directly to the memory database for cross-session search.

### Manifest format

ndx uses the same YAML manifest format as [kcp-commands](https://github.com/Cantara/kcp-commands). Manifests are resolved with a three-tier lookup:

1. `.kcp/commands/<key>.yaml` — project-local (check into repo)
2. `~/.kcp/commands/<key>.yaml` — user-level customizations
3. `~/.ndx/commands/<key>.yaml` — bundled (downloaded by `ndx install`)

## How It Works

1. On first query, the daemon starts and walks the project tree, building a file metadata index plus trigram content index in [redb](https://github.com/cberner/redb). Subsequent startups are incremental — only changed files (by mtime/size) are re-indexed, with trigram extraction parallelized via rayon
2. A debounced filesystem watcher batches events (200ms window) and processes them in bulk transactions
3. CLI commands connect to the daemon via Unix domain socket for index queries
4. The global memory database at `~/.ndx/memory.redb` indexes session transcripts from `~/.claude/projects/`
5. Content search extracts trigrams from queries to narrow candidates before confirming matches
6. The hook subcommand resolves manifests and responds in <20ms

## Data Storage

| Database | Location | Contents |
|----------|----------|----------|
| Project index | `{project}/.ndx/index.redb` | File metadata, trigram content index |
| Recall palace | `{project}/.ndx/recall.redb` | Drawers, rooms, links, embeddings, BM25 index, xrefs, wake state |
| Per-project identity | `{project}/.ndx/identity.toml` | Optional per-project identity override (TOML) |
| Daemon socket | `{project}/.ndx/ndx.sock` | Unix domain socket for client-daemon IPC |
| Daemon log | `{project}/.ndx/ndx.log` | Daemon stderr output |
| Global memory | `~/.ndx/memory.redb` | Sessions, events, agents, cross-references |
| Global identity | `~/.ndx/identity.toml` | Base identity file (TOML), merged with project override |
| Embedding model | `~/.ndx/models/` | Cached `all-MiniLM-L6-v2` ONNX model (~90 MiB, downloaded on first use) |
| Manifests | `~/.ndx/commands/*.yaml` | Command syntax and filter definitions |

## Environment Variables

- `RUST_LOG` — control log verbosity (e.g. `RUST_LOG=ndx=debug`). Daemon logs go to `.ndx/ndx.log`.

## License

MIT. See [LICENSE](LICENSE).

## Acknowledgments

ndx's episodic memory, command manifest, and recall palace features are inspired by and compatible with:

- **[kcp-commands](https://github.com/Cantara/kcp-commands)** — Command syntax injection and output filtering for AI coding agents. Created by [Cantara](https://github.com/Cantara). ndx uses the same YAML manifest format and downloads the same bundled manifests (289 commands covering git, docker, kubectl, cloud CLIs, build tools, and more).

- **[kcp-memory](https://github.com/Cantara/kcp-memory)** — Episodic memory daemon for AI coding sessions. Created by [Cantara](https://github.com/Cantara). ndx implements equivalent session transcript parsing and CLI interfaces in Rust.

- **[Knowledge Context Protocol](https://github.com/Cantara/knowledge-context-protocol)** — The KCP specification that defines the manifest format and integration patterns.

- **[mempalace](https://github.com/milla-jovovich/mempalace)** — The structured memory palace concept (wings, rooms, drawers, 4-layer retrieval ladder, raw verbatim storage) that inspired ndx's `recall` subsystem. Created by [milla-jovovich & Ben Sigman](https://github.com/milla-jovovich). ndx re-implements the useful ideas in Rust on top of `redb` and `fastembed`, deliberately omitting mempalace's AAAK compression layer and MCP server. The 4-layer retrieval ladder, the importance-weighted taxonomy, and the `all-MiniLM-L6-v2` embedding choice (for benchmark parity) come directly from mempalace.

kcp-commands and kcp-memory are licensed under [Apache 2.0](https://www.apache.org/licenses/LICENSE-2.0). mempalace is licensed under MIT. ndx is an independent Rust implementation of the same concepts and protocols. The YAML manifest files downloaded by `ndx install` are redistributed from kcp-commands under their original Apache 2.0 license.
