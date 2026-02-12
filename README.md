# ndx

Persistent file index server for [Claude Code](https://docs.anthropic.com/en/docs/claude-code) via [MCP](https://modelcontextprotocol.io/) (Model Context Protocol). Watches the filesystem, keeps a trigram-based content index current in real time, and serves tools over stdio JSON-RPC.

## Features

- **Fast file listing** — indexed file tree with prefix and glob filtering
- **Trigram content search** — line-level positions for literal queries, regex with trigram-accelerated candidate narrowing
- **Real-time updates** — filesystem watcher re-indexes on create/modify/delete
- **Gitignore-aware** — respects `.gitignore` rules, rebuilds matcher on changes
- **Zero configuration** — single binary, no external services

## Installation

### From source

Requires [Rust](https://rustup.rs/) 1.70+.

```sh
git clone https://github.com/anthropics/ndx.git
cd ndx
cargo build --release
```

The binary is at `target/release/ndx`. Copy it somewhere on your `PATH`:

```sh
cp target/release/ndx ~/.local/bin/
```

### Verify

```sh
ndx help
```

## Setup

### Automatic (recommended)

Run `ndx init` in your project root to create a `.mcp.json` file that Claude Code picks up automatically:

```sh
cd /path/to/your/project
ndx init
```

This creates `.mcp.json` with the ndx server configured. If `.mcp.json` already exists, the `ndx` entry is added/updated without disturbing other servers.

You can also point to a different directory:

```sh
ndx init /path/to/your/project
```

### Manual

Create `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "ndx": {
      "command": "/path/to/ndx",
      "args": ["."]
    }
  }
}
```

## Usage

ndx runs as an MCP server on stdio. Claude Code starts it automatically when it finds `.mcp.json`.

### CLI

```
ndx [path]       Start MCP server for the given project root (default: .)
ndx init [path]  Create .mcp.json for Claude Code in the given directory (default: .)
ndx help         Show this help message
```

### MCP Tools

Once connected, Claude Code can use these tools:

#### `list_files`

List indexed files, optionally filtered by directory prefix.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `path` | string | no | Directory prefix to filter by |

#### `search_files`

Find files matching a glob pattern.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Glob pattern to match file paths |

#### `search_content`

Search file contents by text or regex pattern. Uses a trigram index for fast candidate filtering with line-level positions for literal queries, and full regex match for patterns.

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `pattern` | string | yes | Text or regex pattern to search for |
| `file_pattern` | string | no | Glob to filter which files to search |
| `max_results` | number | no | Maximum results to return (default: 100) |

#### `index_status`

Show index statistics: file count, content-indexed count, unique trigrams, and project root. No parameters.

## How It Works

1. On startup, ndx walks the project tree (respecting `.gitignore`) and builds a file metadata index plus a trigram content index in an embedded database ([redb](https://github.com/cberner/redb))
2. A filesystem watcher keeps the index current as files change
3. Content search extracts 3-byte trigrams from the query to narrow candidate files and lines before confirming with the actual pattern match
4. The database is stored at `{project_root}/.ndx/index.redb` — add `.ndx/` to your `.gitignore`

## Environment Variables

- `RUST_LOG` — control log verbosity (e.g. `RUST_LOG=ndx=debug`). Logs go to stderr.
