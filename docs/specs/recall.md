# ndx recall — Design Specification

**Status:** Draft (0.5.0)
**Last updated:** 2026-04-08
**Location in repo:** `docs/specs/recall.md`

> This is the single source of truth for the `ndx recall` subsystem.
> Requirements are identified by stable `R-NNN` anchors and referenced from
> phase verification checklists (see §16). Implementation deviations are
> recorded in the **Amendment Log** (§18), not made silently.

---

## 1. Overview

`ndx recall` adds a per-project **structured episodic memory palace** on top of
the existing ndx stack. It takes the useful ideas from
[mempalace](https://github.com/milla-jovovich/mempalace) (structured memory,
4-layer retrieval ladder, importance weighting, raw verbatim storage) and
re-implements them in Rust on top of `redb`, reusing ndx's existing trigram
index, session memory, and file watcher infrastructure. It deliberately omits
AAAK, MCP, ChromaDB, and most of mempalace's heuristic NLP — judgment work
(classification, scoring, deduplication, contradiction detection) is delegated
to Claude Code via skills.

The subsystem ships under the command surface `ndx recall` and extends
`ndx xref`.

## 2. Goals

- **G-1** — Provide a durable, per-project store of curated memory items
  ("drawers") with room-based taxonomy and importance weighting.
- **G-2** — Support the mempalace 4-layer retrieval ladder (Identity,
  Essential, On-Demand, Deep Search), rescoped to a per-project palace.
- **G-3** — Hybrid semantic + lexical deep search: local `all-MiniLM-L6-v2`
  embeddings fused with the existing ndx trigram index via Reciprocal Rank
  Fusion.
- **G-4** — Parity with mempalace's benchmarked recall model (same embedding
  model, raw verbatim storage).
- **G-5** — Zero new runtime services: palace access is direct (no new
  daemon); the existing file-index daemon is unchanged.
- **G-6** — Use ndx's strengths as a moat over mempalace: file↔drawer
  cross-references, session↔drawer backlinks, git cross-references, trigram
  acceleration.
- **G-7** — Delegate all judgment work to Claude Code via skills that
  round-trip through `ndx recall drawer` CLI commands. No heuristic NLP in
  Rust.
- **G-8** — Preserve the ndx CLI style and ergonomics. No new configuration
  files beyond `identity.toml`.

## 3. Non-Goals

- **NG-1** — No AAAK dialect, no compression layer.
- **NG-2** — No MCP server.
- **NG-3** — No ChromaDB dependency at runtime. (Read-only import from an
  existing mempalace ChromaDB is supported via direct sqlite read.)
- **NG-4** — No global cross-project palace in v1. Identity is global;
  drawers are strictly per-project.
- **NG-5** — No ANN index in v1. Flat cosine scan is sufficient for target
  scale (≤100k drawers per project).
- **NG-6** — No classification/scoring/dedup heuristics in Rust. All
  judgment flows through Claude via skills.
- **NG-7** — No pre-commit git hooks. Git cross-reference is a passive
  command that walks `git log` on demand.
- **NG-8** — No spec for a monorepo `wing` feature in v1. The `wing` field
  is reserved in the schema (nullable) for future use and for mempalace
  import fidelity; the v1 CLI does not expose it.

## 4. Glossary

| Term | Definition |
|---|---|
| **Drawer** | Atomic unit of stored memory. A short piece of text (≤~2KB) with metadata (room, importance, source, timestamps). |
| **Room** | A named topic bucket within a project's palace (e.g. `architecture`, `decisions`, `people`, `unclassified`). Assigned to a drawer. |
| **Wing** | *(Reserved.)* Nullable sub-partition within a project, preserved for mempalace import and future monorepo support. Not exposed in v1 CLI. |
| **Palace** | The collection of drawers, rooms, and links for a single project, stored in `{project}/.ndx/recall.redb`. |
| **Identity** | Global user profile in `~/.ndx/identity.toml`, optionally overridden per-project at `{project}/.ndx/identity.toml`. Renders to L0. |
| **Wake-up text** | L0 (identity) + L1 (essential story) concatenated, ready for prompt injection. |
| **L0, L1, L2, L3** | The four layers of the recall ladder (see §9). |
| **RRF** | Reciprocal Rank Fusion. Merges semantic and lexical ranked lists by summing `1 / (k + rank)`. |
| **Hybrid search** | RRF fusion of fastembed cosine similarity and trigram intersection. |

---

## 5. Data Model

### 5.1 Drawer (R-100)

- **R-101** — Every drawer has a unique `drawer_id: u64`, assigned monotonically
  from a `META.next_drawer_id` counter persisted across process restarts.
- **R-102** — Every drawer stores a `content_hash: [u8; 32]` (BLAKE3 of
  `text`). Ingest operations **MUST** check for hash collisions within the same
  project and dedup by bumping `importance` on the existing drawer rather than
  creating a duplicate.
- **R-103** — Drawer fields:
  ```rust
  struct Drawer {
      id: u64,
      text: String,                  // raw verbatim content, ≤ 8 KiB
      content_hash: [u8; 32],
      room: String,                  // "unclassified" if unset
      wing: Option<String>,          // reserved, typically None
      importance: u8,                // 1..=10, default 5
      source_kind: SourceKind,       // enum below
      source_session_id: Option<String>,
      source_file: Option<PathBuf>,  // project-relative
      source_line: Option<u32>,
      source_commit: Option<String>,
      created_at: i64,               // unix seconds UTC
      updated_at: i64,
      metadata: BTreeMap<String, String>, // free-form, preserved on import
  }
  enum SourceKind { Memory, Chroma, Project, Manual, Hook }
  ```
- **R-104** — `text` **MUST** be stored verbatim. No normalization, lowercasing,
  stemming, or truncation at storage time.
- **R-105** — `importance` is an integer 1..=10. Default on ingest is 5.
  Manual and skill-based updates may set any value in range.
- **R-106** — `updated_at` is refreshed whenever any field other than
  `created_at` changes.

### 5.2 Room (R-110)

- **R-111** — A room is identified by `(project, name)`. Room names are
  lowercase ASCII, `[a-z0-9_-]+`, max 64 chars.
- **R-112** — The reserved room `unclassified` always exists and is the
  default target for ingestion.
- **R-113** — Room metadata: `title: String`, `description: String`.
  Both optional for `unclassified`.
- **R-114** — Deleting a room is allowed only when it contains zero drawers.
  Otherwise the CLI returns exit code 2 with a clear message.

### 5.3 Links (R-120)

- **R-121** — A link is an edge `(from: u64, to: u64, kind: LinkKind)`.
- **R-122** — Link kinds:
  ```rust
  enum LinkKind { References, Contradicts, Supersedes, DerivedFrom }
  ```
- **R-123** — Links are directional. `Supersedes(a, b)` means `a` replaces `b`;
  `b` **MUST NOT** appear in L1 wake-up output once such a link exists.
- **R-124** — Deleting a drawer cascades to delete all links touching it.

### 5.4 Embeddings (R-130)

- **R-131** — Every drawer has exactly one embedding `[f32; 384]`, stored in a
  separate table from the drawer metadata so that metadata scans (L1, L2) do
  not pay the embedding I/O cost.
- **R-132** — Embeddings are produced by `all-MiniLM-L6-v2` via `fastembed-rs`.
  Re-embedding on model change is a manual operation (`ndx recall reembed`),
  not automatic.
- **R-133** — On ingest, the drawer row is written first, then the embedding
  row in the same transaction. A drawer without an embedding is invalid state
  and **MUST** be treated as an error by the search pipeline.
- **R-134** — On import from a mempalace ChromaDB with matching 384-dim
  embeddings, existing embeddings are reused verbatim (no re-embedding).

### 5.5 Trigram Index (R-140)

- **R-141** — The existing `trigram.rs` extraction is reused. A drawer is
  indexed by extracting trigrams from its `text`.
- **R-142** — Posting list: `trigram_hash: u32 → drawer_id: u64` (many-to-many).
- **R-143** — Trigram insert/delete is transactional with drawer insert/delete.

### 5.6 Cross-Reference Indexes (R-150)

- **R-151** — `file_drawer_xref`: `source_file (project-relative String)
  → Vec<drawer_id>`. Maintained on drawer insert/delete/update.
- **R-152** — `session_drawer_xref`: `source_session_id: String →
  Vec<drawer_id>`. Maintained on drawer insert/delete/update.
- **R-153** — `commit_drawer_xref`: `source_commit: String → Vec<drawer_id>`.
  Populated by `ndx recall xref git` (passive, on-demand command).

### 5.7 Wake-Up State (R-160)

- **R-161** — `wake_injected`: `claude_session_id: String → timestamp: i64`.
  Records sessions that have already received wake-up injection.
- **R-162** — `ndx recall wake --force` clears the entry for the current
  session and re-injects on the next hook invocation.

### 5.8 Meta Table (R-170)

- **R-171** — `META` table stores `schema_version: u32`, `next_drawer_id: u64`,
  `embedding_model: String`, `created_at: i64`, `last_mined_at: i64`.
- **R-172** — `schema_version` starts at 1. Any migration bumps this.
  The palace **MUST** refuse to open a database whose schema version exceeds
  the binary's supported range, with exit code 3.

---

## 6. Storage Layout

- **R-201** — Per-project palace: `{project}/.ndx/recall.redb`. A single redb
  database containing all tables defined in §5.
- **R-202** — Global identity base: `~/.ndx/identity.toml` (optional).
- **R-203** — Per-project identity override: `{project}/.ndx/identity.toml`
  (optional).
- **R-204** — Global embedding model cache: `~/.ndx/models/` (subdirectory
  `all-MiniLM-L6-v2/`).
- **R-205** — `~/.ndx/memory.redb` (global session memory) is **not** modified
  by the recall subsystem; it is only read by `mine --from-memory`.
- **R-206** — The project-index daemon (`.ndx/ndx.sock`, `.ndx/index.redb`,
  `.ndx/ndx.log`) is unchanged. `recall.redb` is accessed directly by the
  `ndx recall` CLI without going through the daemon.
- **R-207** — Concurrent writers to `recall.redb` from multiple CLI
  invocations are serialized by redb's native locking. No additional
  cross-process coordination is required.

---

## 7. Identity Format

### 7.1 TOML Schema (R-300)

- **R-301** — Identity files are TOML.
- **R-302** — Recognized top-level fields:
  ```toml
  name = "Sergiy"                # string
  role = "Software engineer"     # string
  notes = """                    # string (free prose escape hatch)
  Multi-line free-form context.
  """
  ```
- **R-303** — Recognized structured sections:
  ```toml
  [traits]
  style = "direct, terse"
  prefers = "functional patterns"

  [people.alice]
  relation = "colleague"
  context = "reviews my code"

  [projects.ndx]
  path = "/Users/.../RustProjects/ndx"
  focus = "Rust CLI memory tool"
  ```
- **R-304** — Unknown fields are preserved and rendered under a
  "miscellaneous" section. Identity parsing is lenient: syntax errors
  produce a diagnostic but do not abort recall operations (L0 degrades to an
  error marker).

### 7.2 Merge Rules (R-310)

- **R-311** — Precedence: global `~/.ndx/identity.toml` is the base.
  Per-project `{project}/.ndx/identity.toml` (if present) deep-merges on top.
- **R-312** — Deep merge rules:
  - Tables merge recursively.
  - Arrays replace wholesale (no concatenation).
  - Scalar fields override.
- **R-313** — The merged document is rendered to L0 output (§9.1).

### 7.3 L0 Rendering (R-320)

- **R-321** — L0 output begins with the literal header `## L0 — IDENTITY`.
- **R-322** — Fields are rendered in a fixed order: `name`, `role`, `traits`,
  `people`, `projects.<current>`, `notes`, other sections.
- **R-323** — Only the entry in `[projects.<name>]` whose `path` matches the
  current project is rendered in full; other projects are summarized as a
  single "other projects: X, Y, Z" line if any exist.

---

## 8. CLI Surface

All commands are invoked as `ndx recall <subcommand>` unless otherwise noted.
Unless specified, commands operate on the palace rooted at the current working
directory's enclosing project (walk up for `.ndx/`).

### 8.1 Palace Lifecycle (R-400)

- **R-401** — `ndx recall init` — Create `.ndx/recall.redb` in the current
  project if absent. Create the `unclassified` room. Download the embedding
  model to `~/.ndx/models/` if not already cached. Idempotent.
- **R-402** — `ndx recall status` — Print counts (drawers, rooms, links),
  embedding model status, last mine timestamp, identity status. Human-readable
  by default, `--json` for machine-readable.
- **R-403** — `ndx recall reembed` — Re-compute embeddings for all drawers.
  Used when switching embedding model.

### 8.2 Retrieval (R-410)

- **R-411** — `ndx recall wake [--force]` — Emit L0 + L1 text to stdout. With
  `--force`, clears the current session's wake-up marker first. Exit code 0
  always (empty palace produces a diagnostic L1 section).
- **R-412** — `ndx recall get --room <name> [--limit N] [--json]` — L2
  metadata-filtered retrieval. Default `--limit 10`.
- **R-413** — `ndx recall search "query" [--room X] [--limit N] [--lexical|--semantic|--hybrid] [--json]` —
  L3 search. Default mode is `--hybrid`. `--lexical` restricts to trigram,
  `--semantic` restricts to cosine.
- **R-414** — Search output (text mode) includes per-result: drawer id,
  room, source hint, similarity score (for semantic/hybrid), rank position,
  and a snippet (truncated to 300 chars with ellipsis).

### 8.3 Drawer CRUD (R-420)

- **R-421** — `ndx recall drawer add "text" [--room X] [--importance N] [--source-file F]` —
  Create a drawer. Returns the new `drawer_id` on stdout.
- **R-422** — `ndx recall drawer list [--room X] [--pending <op>] [--limit N] [--json]` —
  List drawers. `--pending` is a filter for skill consumption:
  - `classify` → `room = "unclassified"`
  - `score` → drawers whose `importance` is still 5 AND `source_kind ≠ Manual`
  - `dedupe` → drawers with trigram-overlapping neighbors above a threshold
  - `contradict` → drawers that have at least one incoming
    `contradict-candidate` link (see §11)
  - `summarize` → one representative drawer per non-empty room
- **R-423** — `ndx recall drawer show --id N [--json]` — Full detail
  including links.
- **R-424** — `ndx recall drawer update --id N [--room X] [--importance N] [--text "..."]` —
  Update fields. `updated_at` is refreshed.
- **R-425** — `ndx recall drawer rm --id N` — Delete a drawer and cascade
  links and cross-refs.
- **R-426** — `ndx recall drawer link --from A --to B --kind <kind>` —
  Create a link. Idempotent.
- **R-427** — `ndx recall drawer unlink --from A --to B [--kind <kind>]` —
  Delete matching links.

### 8.4 Room CRUD (R-430)

- **R-431** — `ndx recall room add <name> [--title T] [--description D]`
- **R-432** — `ndx recall room list [--json]`
- **R-433** — `ndx recall room show <name> [--json]`
- **R-434** — `ndx recall room rm <name>` — Fails if room is non-empty (R-114).
- **R-435** — `ndx recall room rename <old> <new>` — Updates all drawers
  in a single transaction.

### 8.5 Identity CRUD (R-440)

- **R-441** — `ndx recall identity show [--merged]` — Print current identity.
  With `--merged`, show the merged view; without, show only the file that
  would be edited by default.
- **R-442** — `ndx recall identity edit [--project]` — Open `$EDITOR` on
  the appropriate identity file (global by default, per-project with
  `--project`). Creates the file with a commented template if absent.

### 8.6 Mining (R-450)

- **R-451** — `ndx recall mine --from-memory [--since <iso8601>]` — Import
  from global `~/.ndx/memory.redb`, filtered by current project path.
- **R-452** — `ndx recall mine --from-chroma <path> [--wing <name>]` —
  Direct sqlite read of a mempalace ChromaDB directory. Preserves
  metadata; reuses embeddings if 384-dim.
- **R-453** — `ndx recall mine --project [--path P]` — Walk the project tree
  (via `ignore` crate, respecting `.gitignore`), paragraph-chunk text files,
  emit drawers. Default path is the project root.
- **R-454** — Every mine command is **idempotent** via content-hash dedup
  (R-102) and **reports** at completion: `added: N`, `deduped: M`,
  `skipped: K`.

### 8.7 Cross-References (R-460)

- **R-461** — `ndx xref drawer <file>` — List drawers whose `source_file`
  matches, or whose text mentions the file.
- **R-462** — `ndx xref drawer-session <session-id>` — List drawers derived
  from or mentioning a specific session.
- **R-463** — `ndx xref git <commit>` — Walk the commit's changed files and
  return drawers referencing any of them. Populates `commit_drawer_xref` as
  a side effect so subsequent lookups of the same commit are instant.

### 8.8 JSON Output Convention (R-470)

- **R-471** — Every command that accepts `--json` emits a single JSON object
  or array to stdout, with no trailing newline or progress output mixed in.
  Progress/diagnostics go to stderr.
- **R-472** — JSON schemas are frozen at release time and documented in §11
  (for skill-facing schemas) and by inline comments elsewhere.

### 8.9 Exit Codes (R-480)

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic error (I/O, parse, etc.) |
| 2 | Constraint violation (e.g. removing non-empty room) |
| 3 | Schema version unsupported |
| 4 | Palace not initialized (run `ndx recall init`) |
| 5 | Model not available (run `ndx recall init` to download) |
| 64 | CLI usage error (bad flags) |

---

## 9. Retrieval Ladder

### 9.1 L0 — Identity (R-500)

- **R-501** — L0 is the rendered merged identity (§7.3). Always loaded in
  wake-up.
- **R-502** — Approximate token budget: 100–300 tokens. No hard cap, but if
  the rendered identity exceeds 1500 tokens the wake-up command emits a
  diagnostic on stderr.

### 9.2 L1 — Essential Story (R-510)

- **R-511** — L1 is generated from the current project's drawers at
  wake-up time. No persistent L1 cache in v1.
- **R-512** — Generation algorithm (v1, deterministic, no LLM):
  1. Load all drawers from the current palace.
  2. Exclude drawers referenced by any `Supersedes(_, this)` link (R-123).
  3. Sort by `importance` descending, `created_at` descending as tiebreak.
  4. Take up to 15 drawers whose combined rendered length ≤ 3200 chars.
  5. Group by room, render rooms in sorted order.
  6. For each drawer, emit: `- <snippet≤200 chars>  (src: <short_source>)`.
- **R-513** — L1 output begins with `## L1 — ESSENTIAL`.
- **R-514** — If the palace is empty, L1 renders `*(no drawers yet — run
  `ndx recall mine --from-memory` to seed)*` and returns exit code 0.
- **R-515** — L1 generation is replaceable: a future release may cache a
  `/ndx-recall-summarize` output and prefer it when present and newer than
  the highest `updated_at` across drawers. Not in v1.

### 9.3 L2 — On-Demand (R-520)

- **R-521** — L2 is `ndx recall get --room <name>`. Pure metadata filter,
  no semantic, no trigram.
- **R-522** — Output is ordered by `importance` desc, `updated_at` desc.
- **R-523** — Output format mirrors L1 drawer rendering but without the
  `## L1` header.

### 9.4 L3 — Hybrid Search (R-530)

- **R-531** — L3 is `ndx recall search "query"`. Default mode is `--hybrid`.
- **R-532** — Hybrid algorithm:
  1. Compute query embedding `q` via fastembed MiniLM-L6-v2.
  2. **Semantic rank:** for every drawer embedding `e_i`, compute cosine
     similarity `sim_i = (q · e_i) / (‖q‖·‖e_i‖)`. Sort descending,
     take top `K_sem = 50`. Produces `rank_sem[drawer_id]`.
  3. **Lexical rank:** extract trigrams from query, intersect posting
     lists, count hits per drawer, sort descending, take top `K_lex = 50`.
     Produces `rank_lex[drawer_id]`.
  4. **Fuse via RRF:** for each drawer in
     `(K_sem ∪ K_lex)`, `score[drawer] = 1/(60 + rank_sem[drawer])
     + 1/(60 + rank_lex[drawer])` (missing ranks contribute 0).
  5. Sort by fused score descending, take top `N_out` (default 10).
- **R-533** — `--lexical` restricts to steps 3, 5 (no fusion).
- **R-534** — `--semantic` restricts to steps 1, 2, 5 (no fusion).
- **R-535** — Room filter `--room X` applies **before** ranking (prunes
  candidates), not after.
- **R-536** — Empty result set is not an error; exit code 0 with
  `*(no matches)*` text output or `{"results": []}` JSON.

---

## 10. Mining

### 10.1 `mine --from-memory` (R-600)

- **R-601** — Read sessions from `~/.ndx/memory.redb` where the session's
  project path matches the current project's canonical path.
- **R-602** — For each matched session, walk turn-pairs (one user message +
  the immediately following assistant response). Emit one drawer per
  turn-pair.
- **R-603** — Drawer `text` = `"USER: {user}\n\nASSISTANT: {assistant}"`,
  truncated to 8 KiB if necessary with a trailing `… [truncated]` marker.
- **R-604** — Drawer metadata:
  - `source_kind = Memory`
  - `source_session_id = <session id>`
  - `source_file = None`
  - `room = "unclassified"`
  - `importance = 5`
- **R-605** — `--since <iso8601>` filters to sessions whose `started_at`
  is ≥ the given timestamp.

### 10.2 `mine --from-chroma` (R-610)

- **R-611** — Open the target directory as a ChromaDB SQLite database via
  `rusqlite` read-only. The schema used by ChromaDB ≥0.5 is documented
  inline in the implementation.
- **R-612** — Iterate documents in the `mempalace_drawers` collection.
  For each: map the document text to `drawer.text`, preserve the metadata
  map in `drawer.metadata`, lift known keys into typed fields:
  - `metadata.wing → drawer.wing`
  - `metadata.room → drawer.room` (or `unclassified` if missing)
  - `metadata.source_file → drawer.source_file`
  - `metadata.importance` / `weight` / `emotional_weight` → `drawer.importance`
    (clamped to 1..=10, rounded)
- **R-613** — If the source embedding exists and is 384-dim, reuse it
  verbatim. Otherwise, re-embed with fastembed.
- **R-614** — `--wing <name>` optionally restricts import to drawers whose
  wing matches.

### 10.3 `mine --project` (R-620)

- **R-621** — Walk the current project's file tree via the `ignore` crate
  (same rules as the existing file-index scanner).
- **R-622** — For each text file whose size ≤ 1 MiB, split into paragraphs
  at blank-line boundaries. Paragraphs exceeding 2 KiB are split further at
  sentence boundaries.
- **R-623** — Emit one drawer per paragraph with:
  - `source_kind = Project`
  - `source_file = <project-relative>`
  - `source_line = <1-based first line of paragraph>`
  - `room = "unclassified"`
  - `importance = 5`
- **R-624** — Skip files with extensions in a denylist
  (`.lock`, `.min.js`, `.map`, `.pb`, etc.) maintained alongside the
  existing scanner's rules.

### 10.4 Ingest Invariants (R-630)

- **R-631** — Every mine command **MUST** run in a single redb write
  transaction per batch of ≤1000 drawers to bound memory and provide
  crash safety.
- **R-632** — On partial failure (e.g., embedding model unavailable
  mid-mine), drawers already written in earlier transactions remain;
  a diagnostic reports the failed batch boundary.
- **R-633** — Content-hash dedup (R-102) applies: re-mining an unchanged
  source produces `added: 0, deduped: N`.

---

## 11. Skills Contract

Skills are Claude Code slash commands at `.claude/commands/ndx-recall-*.md`.
Each skill reads from ndx via `ndx recall drawer list --pending <op> --json`
and writes back via `ndx recall drawer update|link|rm`. No skill has direct
database access; all mutations are CLI round-trips.

### 11.1 Shared Read Schema (R-700)

- **R-701** — `ndx recall drawer list --pending <op> --json` emits:
  ```json
  {
    "op": "classify",
    "project": {
      "path": "/Users/.../ndx",
      "existing_rooms": ["architecture", "decisions", "unclassified"]
    },
    "drawers": [
      {
        "id": 42,
        "text": "...",
        "room": "unclassified",
        "importance": 5,
        "source_kind": "Memory",
        "source_session_id": "abc",
        "source_file": "src/main.rs",
        "source_line": 123,
        "created_at": 1712000000,
        "updated_at": 1712000000,
        "links_in": [],
        "links_out": []
      }
    ],
    "cursor": "opaque-token-or-null"
  }
  ```
- **R-702** — `--limit N` caps the drawers array. A non-null `cursor` signals
  more results; pass `--cursor <token>` to continue.

### 11.2 Shared Write Commands (R-710)

- **R-711** — `ndx recall drawer update --id N [--room X] [--importance N] [--text "..."]`
- **R-712** — `ndx recall drawer link --from A --to B --kind <kind>`
- **R-713** — `ndx recall drawer rm --id N`
- **R-714** — All write commands emit a single JSON object `{"ok": true,
  "id": N}` (or `{"ok": false, "error": "..."}`) on stdout when
  `--json` is passed.

### 11.3 Skill: `/ndx-recall-classify` (R-720)

- **R-721** — Purpose: assign rooms to drawers currently in `unclassified`.
- **R-722** — Flow: fetch pending batch → propose `room` per drawer, possibly
  creating new rooms via `ndx recall room add` → update each drawer.
- **R-723** — Success criterion: all drawers in the batch have `room ≠ "unclassified"`.

### 11.4 Skill: `/ndx-recall-score` (R-730)

- **R-731** — Purpose: assign meaningful `importance` to drawers currently
  at default 5.
- **R-732** — Flow: fetch pending batch → score each drawer 1..=10 based on
  the drawer's content and project context → update.

### 11.5 Skill: `/ndx-recall-dedupe` (R-740)

- **R-741** — Purpose: merge near-duplicate drawers.
- **R-742** — Flow: fetch pending batch (pairs with high trigram overlap) →
  judge merges → for each merge, call `drawer update` on the survivor
  (bumping importance by the merged drawer's importance up to 10) and
  `drawer rm` on the redundant one.

### 11.6 Skill: `/ndx-recall-contradict` (R-750)

- **R-751** — Purpose: identify contradictions and record them as
  `LinkKind::Contradicts`.
- **R-752** — Candidate discovery: ndx pre-computes candidate pairs as
  drawers sharing ≥ K trigrams (K configurable, default 8) and emits them
  via `--pending contradict`. The skill judges whether each pair is an
  actual contradiction and creates links accordingly.

### 11.7 Skill: `/ndx-recall-summarize` (R-760)

- **R-761** — Purpose: generate a higher-quality L1 essential-story text
  than the naive v1 algorithm, cached as drawers in a `_summary_` room.
- **R-762** — Flow: fetch room-grouped representative drawers →
  synthesize one concise summary per active room → `drawer add --room _summary_`
  with `importance = 10`.
- **R-763** — *(v1 consumes the naive L1 algorithm only; this skill exists
  as a write-path in v1 and will be consumed by L1 in a future release. See
  R-515.)*

---

## 12. Hook Integration

### 12.1 Wake-Up Injection (R-800)

- **R-801** — The existing `ndx hook bash-pre` subcommand is extended to
  optionally inject wake-up text once per Claude session.
- **R-802** — The hook reads the Claude session ID from the PreToolUse
  hook payload (specifically the `session_id` field passed by Claude Code
  settings.json hook contract).
- **R-803** — Before emitting normal syntax hints, the hook checks
  `wake_injected` (R-161). If the current session ID is absent, it:
  1. Generates wake-up text via the same path as `ndx recall wake`.
  2. Prefixes the hook output with the wake-up text wrapped in a marker
     block: `# ndx-recall wake-up (session abc…)\n{wake text}\n# /wake-up`.
  3. Inserts the session ID into `wake_injected` with the current timestamp.
- **R-804** — The wake-up injection **MUST NOT** fail the hook on error
  (missing palace, model unavailable, etc.). Errors go to stderr; the hook
  proceeds with normal syntax hints.
- **R-805** — `ndx recall wake --force` clears the current session's
  `wake_injected` entry so the next Bash command re-injects.

### 12.2 Opportunistic Drawer Capture (R-810)

- **R-811** — *(Tangential, lower priority.)* The hook MAY scan command
  output for marker phrases ("decided to", "because", "won't work
  because") and auto-file drawers into a `_scratch_` room with
  `importance = 3`, `source_kind = Hook`.
- **R-812** — This behavior is **off by default** in v1. Enabled via
  `ndx recall config --hook-capture on`. (Config is stored in `META`.)

---

## 13. Cross-References

### 13.1 File ↔ Drawer (R-900)

- **R-901** — `ndx xref drawer <file>` walks `file_drawer_xref` (R-151)
  for drawers with matching `source_file`, then walks the trigram index
  for drawers whose `text` mentions the file's basename.
- **R-902** — Results are deduplicated by drawer_id and ordered by
  `importance` desc, `updated_at` desc.

### 13.2 Session ↔ Drawer (R-910)

- **R-911** — `ndx xref drawer-session <session-id>` returns drawers with
  `source_session_id == <id>`.
- **R-912** — The inverse (drawer → session) is already available via
  `drawer show`; no new command.

### 13.3 Commit ↔ Drawer (R-920)

- **R-921** — `ndx xref git <commit>` is a passive on-demand command:
  1. Walk `git diff-tree --name-only <commit>` for changed files.
  2. For each file, look up drawers via `file_drawer_xref` and trigram
     basename match.
  3. Dedupe and return.
- **R-922** — Side effect: the resulting drawer ids are written to
  `commit_drawer_xref[<commit>]` so subsequent calls for the same commit
  are O(1).
- **R-923** — No git hooks are installed. The user runs this command when
  they want to see "what is the stored *why* for this change".

---

## 14. Error Handling

- **R-1001** — All errors propagate to the CLI boundary and map to an
  exit code from R-480. Error messages are human-readable on stderr.
- **R-1002** — `--json` output never contains free-form error text; errors
  in `--json` mode emit `{"ok": false, "error": "...", "code": N}` and the
  corresponding exit code.
- **R-1003** — Panics are bugs. The CLI catches them at `main` and emits
  exit code 1 with a "please report" footer; no unwinding past main.

---

## 15. Dependencies

- **R-1100** — New crates:
  - `fastembed = "4"` — local embeddings (MiniLM-L6-v2)
  - `toml = "0.8"` — identity file parsing
  - `rusqlite = { version = "0.32", features = ["bundled"] }` — mempalace
    import (read-only)
  - `blake3 = "1"` — content hashing
- **R-1101** — `ort` (onnxruntime) is pulled in transitively by fastembed.
  Acceptable.
- **R-1102** — No new runtime services. No MCP, no ChromaDB, no Python.

---

## 16. Implementation Phases

Each phase closes with a **Verification step**: re-read the referenced
requirement anchors against the delivered code and tests, and record in §18
either "conforms" or an amendment describing the deviation.

### Phase 0 — Spec (current)

- **Deliverable:** this document, reviewed and approved.
- **Verify:** user sign-off on the spec before Phase 1 starts.

### Phase 1 — Foundation

- **Scope:** redb schema (all tables from §5), drawer/room CRUD primitives
  in-code, `ndx recall init`, `ndx recall status`, CWD project detection,
  identity TOML parser + merge + L0 rendering, exit codes.
- **Requirements:** R-100..R-172, R-201, R-207, R-301..R-323, R-401, R-402,
  R-431..R-435, R-441, R-442, R-480, R-1001..R-1003.
- **Verify:** spec-conformance check of schema and L0 rendering against
  R-100 series and R-300 series.

### Phase 2 — First Data In

- **Scope:** `mine --from-memory`, `mine --from-chroma`, `mine --project`,
  content-hash dedup, mining invariants.
- **Requirements:** R-102, R-451..R-454, R-600..R-633.
- **Verify:** run each mine mode on representative data (this repo's own
  memory, a mempalace export if available, this repo's source tree) and
  compare counts to expected.

### Phase 3 — Retrieval Ladder

- **Scope:** `wake`, `get`, `search` (hybrid, lexical, semantic), fastembed
  model loading, trigram integration, RRF fusion.
- **Requirements:** R-130..R-142, R-411..R-414, R-500..R-536.
- **Verify:** hand-crafted queries against a seeded palace; verify
  hybrid beats either alone on at least one synonym query.

### Phase 4 — Cross-References

- **Scope:** `xref drawer`, `xref drawer-session`, `xref git`,
  maintenance of `file_drawer_xref`/`session_drawer_xref`/`commit_drawer_xref`.
- **Requirements:** R-150..R-153, R-461..R-463, R-900..R-923.
- **Verify:** cross-ref round-trips (file → drawers → session → drawers).

### Phase 5 — Hook Wake-Up Injection

- **Scope:** extend `ndx hook bash-pre` to inject wake-up text once per
  Claude session, `wake_injected` table maintenance, `wake --force`.
- **Requirements:** R-160..R-162, R-800..R-805.
- **Verify:** simulate two consecutive hook invocations in the same
  session; wake-up appears in the first only.

### Phase 6 — Skills

- **Scope:** five skill files (`ndx-recall-classify`, `-score`, `-dedupe`,
  `-contradict`, `-summarize`), `drawer list --pending <op> --json`
  implementation, write-back command JSON output.
- **Requirements:** R-420..R-427, R-470..R-472, R-700..R-763.
- **Verify:** each skill runs end-to-end against a seeded palace,
  producing visible state changes via CLI.

### Phase 7 — Polish

- **Scope:** update `~/.claude/commands/ndx.md` skill with new surface,
  README "Recall" section, CHANGELOG finalization, install path
  updates, version sanity.
- **Requirements:** all previous phases conform. No new R-IDs.
- **Verify:** fresh checkout, `cargo build --release`, full walkthrough
  from `ndx recall init` to `ndx recall wake` on a real project.

---

## 17. Open Questions

*(Empty at spec approval time. New questions get appended with date;
answered questions get moved to the amendment log.)*

---

## 18. Amendment Log

*(Append-only. Each entry: date, phase, requirement IDs touched,
description, rationale.)*

- **2026-04-08** — Initial draft (Phase 0 start).

- **2026-04-08** — Phase 1 delivered. Conformance check against
  R-100..R-172, R-201, R-207, R-301..R-323, R-401, R-402, R-431..R-435,
  R-441, R-442, R-480, R-1001..R-1003 completed. Core model, identity,
  and Phase 1 CLI surface pass. 11 unit tests green. Deferrals recorded
  below; none are silent divergences — each is an explicit scope boundary
  for a later phase.

- **2026-04-08 / Phase 1 / R-103** — The `content_hash` field on `Drawer`
  is serialized as a hex `String` (64 chars, BLAKE3 hex) rather than a
  raw `[u8; 32]` byte array. Rationale: drawer rows are stored as
  `serde_json` bytes in the `DRAWERS` table; `[u8; 32]` serializes to a
  32-element numeric array which is less readable on `ndx recall drawer
  show --json` and wastes space compared to the hex string. The 32 bytes
  of entropy are preserved. The `DRAWER_BY_HASH` index key is still the
  raw 32-byte BLAKE3 digest for compact lookups. No behavioral change to
  R-102 dedup.

- **2026-04-08 / Phase 1 / R-124** — Link cascade on drawer deletion is
  implemented in Phase 2 alongside the `ndx recall drawer rm` CLI. Phase 1
  does not expose drawer deletion, so no divergence is observable.

- **2026-04-08 / Phase 1 / R-151, R-152** — Insert-side population of
  `file_drawer_xref` and `session_drawer_xref` is implemented.
  Delete-side maintenance is implemented in Phase 2 with `drawer rm`.

- **2026-04-08 / Phase 1 / R-401** — Embedding model download during
  `ndx recall init` is deferred to Phase 3 when `fastembed` is added.
  Phase 1 `init` is otherwise complete (palace creation, unclassified
  room, idempotent). `ndx recall status` shows
  `Embedding model: (none — Phase 3)` until then.

- **2026-04-08 / Phase 1 / R-1002** — Structured JSON error envelopes
  (`{"ok": false, "error": "...", "code": N}`) for `--json` commands
  are deferred to Phase 6 when most `--json` surface arrives. Phase 1
  only ships `status --json` and `room list --json`, neither of which
  surfaces meaningful errors beyond "palace not initialized" (caught
  before the JSON path). Human-readable error text is emitted on stderr
  and the exit code is set correctly, so scripts can already distinguish
  failure modes.

- **2026-04-08 / Phase 1 / R-1003** — Explicit panic hook with a "please
  report" footer and forced exit code 1 is deferred. Rust's default
  panic behavior (exit code 101, panic message on stderr) is in effect.
  No functional impact in Phase 1; no panic paths exist in the recall
  code.
