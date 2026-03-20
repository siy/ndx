# ndx — Fast File Index & Memory Search

Use the `ndx` CLI for trigram-accelerated file search, project file listing, session memory queries, and cross-referencing. ndx is available via Bash and works in all contexts including subagents.

## When to use ndx

- **Content search across many files** — faster than grep for large codebases due to trigram index
- **Session memory** — find what was discussed or done in previous Claude Code sessions
- **Cross-referencing** — find which sessions touched a file, or what files a session modified

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
ndx memory context --project /path/to/project    # for a specific project
ndx memory subagents "search query"              # search subagent transcripts
ndx memory subagents "query" --parent <id>       # filter by parent session
ndx memory tree <session-id>                     # session + subagent tree
```

All memory commands accept `--limit N` to control result count.

## Cross-Reference Commands

```bash
ndx xref file src/main.rs                # find sessions that touched this file
ndx xref session <session-id>            # list files touched by a session
```

## Maintenance

```bash
ndx scan              # re-scan project index + memory database
ndx install           # download command manifests, register hook + skill
ndx init              # install ndx skill into current project
```
