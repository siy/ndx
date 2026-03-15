use crate::index::Index;
use crate::memory::{self, MemoryIndex};
use crate::trigram;
use chrono::DateTime;
use globset::Glob;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::Searcher;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::tool;
use rmcp::ServerHandler;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

pub struct NdxServer {
    index: Arc<Index>,
    memory: Option<Arc<MemoryIndex>>,
    tool_router: ToolRouter<Self>,
}

impl NdxServer {
    pub fn new(index: Arc<Index>, memory: Option<Arc<MemoryIndex>>) -> Self {
        Self {
            index,
            memory,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ListFilesInput {
    /// Optional directory prefix to filter by
    pub path: Option<String>,
    /// Optional glob pattern to filter files
    pub pattern: Option<String>,
    /// Sort order: "name" (default) or "modified" (newest first)
    pub sort: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchFilesInput {
    /// Glob pattern to match file paths
    pub pattern: String,
    /// Sort order: "name" (default) or "modified" (newest first)
    pub sort: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchContentInput {
    /// Text or regex pattern to search for
    pub pattern: String,
    /// Optional glob to filter which files to search
    pub file_pattern: Option<String>,
    /// Maximum number of results (default: 100)
    pub max_results: Option<usize>,
    /// Number of context lines before each match
    pub before_context: Option<usize>,
    /// Number of context lines after each match
    pub after_context: Option<usize>,
    /// Output mode: "content" (default), "files_with_matches", "count"
    pub output_mode: Option<String>,
    /// Skip first N results for pagination
    pub offset: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemorySearchInput {
    /// Text to search for in session transcripts
    pub query: String,
    /// Maximum number of results (default: 20)
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemoryEventsSearchInput {
    /// Text to search for in event commands
    pub query: String,
    /// Maximum number of results (default: 50)
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemoryListInput {
    /// Filter by project directory
    pub project: Option<String>,
    /// Maximum number of results (default: 20)
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemoryStatsInput {}

#[derive(Deserialize, JsonSchema)]
pub struct MemorySessionDetailInput {
    /// Session ID to look up
    pub session_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemoryProjectContextInput {
    /// Project directory to get context for
    pub project: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemorySubagentSearchInput {
    /// Text to search for in subagent transcripts
    pub query: String,
    /// Filter by parent session ID
    pub parent_session_id: Option<String>,
    /// Maximum number of results (default: 20)
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MemorySessionTreeInput {
    /// Session ID to get the tree for
    pub session_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct FileSessionsInput {
    /// File path to find sessions for
    pub path: String,
    /// Maximum number of results (default: 10)
    pub limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SessionFilesInput {
    /// Session ID to list files for
    pub session_id: String,
}

/// Get candidate files with line hints for content search using trigram index.
/// Includes non-content-indexed files (binary-skipped) when trigrams narrow candidates.
fn get_search_candidates(
    index: &Index,
    pattern: &str,
    file_pattern: Option<&str>,
) -> Result<Vec<(String, Vec<u32>)>, anyhow::Error> {
    let query = trigram::extract_longest_literal(pattern).unwrap_or(pattern);

    let mut results = match index.search_trigram(query)? {
        Some(candidates) => {
            let mut all = candidates;
            for path in index.list_non_content_indexed()? {
                all.push((path, Vec::new()));
            }
            all
        }
        None => all_files_no_lines(index)?,
    };

    if let Some(fp) = file_pattern {
        let glob = Glob::new(fp)?.compile_matcher();
        results.retain(|(p, _)| glob.is_match(p.as_str()));
    }

    Ok(results)
}

fn all_files_no_lines(index: &Index) -> Result<Vec<(String, Vec<u32>)>, anyhow::Error> {
    Ok(index
        .list_all()?
        .into_iter()
        .map(|p| (p, Vec::new()))
        .collect())
}

fn format_modified(epoch_secs: u64) -> String {
    DateTime::from_timestamp(epoch_secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| epoch_secs.to_string())
}

/// Format context lines around matches using grep-style output.
/// Match lines: `file:line:content`, context lines: `file:line-content`, separators: `--`
fn format_with_context(
    file_path: &str,
    match_line_nums: &[usize],
    lines: &[&str],
    before: usize,
    after: usize,
) -> Vec<String> {
    if match_line_nums.is_empty() {
        return Vec::new();
    }

    let total = lines.len();

    // Build merged ranges: (start_1idx, end_1idx, match_lines_in_range)
    let mut ranges: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    for &m in match_line_nums {
        let start = m.saturating_sub(before).max(1);
        let end = (m + after).min(total);
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 + 1 {
                last.1 = end;
                last.2.push(m);
                continue;
            }
        }
        ranges.push((start, end, vec![m]));
    }

    let mut output = Vec::new();
    for (i, (start, end, match_set)) in ranges.iter().enumerate() {
        if i > 0 {
            output.push("--".to_string());
        }
        for ln in *start..=*end {
            let content = lines[ln - 1].trim_end();
            if match_set.contains(&ln) {
                output.push(format!("{}:{}:{}", file_path, ln, content));
            } else {
                output.push(format!("{}:{}-{}", file_path, ln, content));
            }
        }
    }
    output
}

// ── Match-finding helpers ──

fn find_literal_matches(
    index: &Index,
    file_path: &str,
    pattern: &str,
    line_hints: &[u32],
) -> Vec<(usize, String)> {
    let abs_path = index.abs_path(file_path);
    let content = match std::fs::read_to_string(&abs_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut matches = Vec::new();

    if line_hints.is_empty() {
        for (idx, line) in lines.iter().enumerate() {
            if line.contains(pattern) {
                matches.push((idx + 1, line.trim_end().to_string()));
            }
        }
    } else {
        for &ln in line_hints {
            let idx = (ln as usize).saturating_sub(1);
            if idx < lines.len() && lines[idx].contains(pattern) {
                matches.push((ln as usize, lines[idx].trim_end().to_string()));
            }
        }
    }
    matches
}

fn find_regex_matches(
    index: &Index,
    file_path: &str,
    matcher: &RegexMatcher,
) -> Vec<(usize, String)> {
    let abs_path = index.abs_path(file_path);
    let mut searcher = Searcher::new();
    let mut matches = Vec::new();
    let _ = searcher.search_path(
        matcher,
        &abs_path,
        UTF8(|lnum, line| {
            matches.push((lnum as usize, line.trim_end().to_string()));
            Ok(true)
        }),
    );
    matches
}

fn has_literal_match(index: &Index, file_path: &str, pattern: &str, line_hints: &[u32]) -> bool {
    let abs_path = index.abs_path(file_path);
    let content = match std::fs::read_to_string(&abs_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    if line_hints.is_empty() {
        content.contains(pattern)
    } else {
        let lines: Vec<&str> = content.lines().collect();
        line_hints.iter().any(|&ln| {
            let idx = (ln as usize).saturating_sub(1);
            idx < lines.len() && lines[idx].contains(pattern)
        })
    }
}

fn has_regex_match(index: &Index, file_path: &str, matcher: &RegexMatcher) -> bool {
    let abs_path = index.abs_path(file_path);
    let mut searcher = Searcher::new();
    let mut found = false;
    let _ = searcher.search_path(
        matcher,
        &abs_path,
        UTF8(|_, _| {
            found = true;
            Ok(false)
        }),
    );
    found
}

fn count_literal_matches(
    index: &Index,
    file_path: &str,
    pattern: &str,
    line_hints: &[u32],
) -> usize {
    let abs_path = index.abs_path(file_path);
    let content = match std::fs::read_to_string(&abs_path) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    if line_hints.is_empty() {
        content.lines().filter(|l| l.contains(pattern)).count()
    } else {
        let lines: Vec<&str> = content.lines().collect();
        line_hints
            .iter()
            .filter(|&&ln| {
                let idx = (ln as usize).saturating_sub(1);
                idx < lines.len() && lines[idx].contains(pattern)
            })
            .count()
    }
}

fn count_regex_matches(index: &Index, file_path: &str, matcher: &RegexMatcher) -> usize {
    let abs_path = index.abs_path(file_path);
    let mut searcher = Searcher::new();
    let mut count = 0usize;
    let _ = searcher.search_path(
        matcher,
        &abs_path,
        UTF8(|_, _| {
            count += 1;
            Ok(true)
        }),
    );
    count
}

// ── Search mode implementations ──

fn search_content_mode(
    index: &Index,
    pattern: &str,
    is_literal: bool,
    candidates: &[(String, Vec<u32>)],
    offset: usize,
    max_results: usize,
    before_ctx: usize,
    after_ctx: usize,
) -> Result<String, String> {
    let has_context = before_ctx > 0 || after_ctx > 0;
    let matcher = if !is_literal {
        Some(RegexMatcher::new(pattern).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let mut results: Vec<String> = Vec::new();
    let mut skipped = 0usize;
    let mut collected = 0usize;

    for (file_path, line_hints) in candidates {
        if collected >= max_results {
            break;
        }

        let file_matches = if is_literal {
            find_literal_matches(index, file_path, pattern, line_hints)
        } else {
            find_regex_matches(index, file_path, matcher.as_ref().unwrap())
        };

        if file_matches.is_empty() {
            continue;
        }

        let mut surviving_line_nums: Vec<usize> = Vec::new();
        let mut surviving_content: Vec<(usize, String)> = Vec::new();

        for (ln, content) in &file_matches {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if collected >= max_results {
                break;
            }
            surviving_line_nums.push(*ln);
            surviving_content.push((*ln, content.clone()));
            collected += 1;
        }

        if surviving_content.is_empty() {
            continue;
        }

        if has_context {
            let abs_path = index.abs_path(file_path);
            if let Ok(content) = std::fs::read_to_string(&abs_path) {
                let lines: Vec<&str> = content.lines().collect();
                if !results.is_empty() {
                    results.push("--".to_string());
                }
                results.extend(format_with_context(
                    file_path,
                    &surviving_line_nums,
                    &lines,
                    before_ctx,
                    after_ctx,
                ));
            } else {
                for (ln, c) in &surviving_content {
                    results.push(format!("{}:{}:{}", file_path, ln, c));
                }
            }
        } else {
            for (ln, c) in &surviving_content {
                results.push(format!("{}:{}:{}", file_path, ln, c));
            }
        }
    }

    Ok(results.join("\n"))
}

fn search_files_mode(
    index: &Index,
    pattern: &str,
    is_literal: bool,
    candidates: &[(String, Vec<u32>)],
    offset: usize,
    max_results: usize,
) -> Result<String, String> {
    let matcher = if !is_literal {
        Some(RegexMatcher::new(pattern).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let mut files: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    for (file_path, line_hints) in candidates {
        if files.len() >= max_results {
            break;
        }

        let has_match = if is_literal {
            has_literal_match(index, file_path, pattern, line_hints)
        } else {
            has_regex_match(index, file_path, matcher.as_ref().unwrap())
        };

        if has_match {
            if skipped < offset {
                skipped += 1;
            } else {
                files.push(file_path.clone());
            }
        }
    }

    Ok(files.join("\n"))
}

fn search_count_mode(
    index: &Index,
    pattern: &str,
    is_literal: bool,
    candidates: &[(String, Vec<u32>)],
    offset: usize,
    max_results: usize,
) -> Result<String, String> {
    let matcher = if !is_literal {
        Some(RegexMatcher::new(pattern).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let mut counts: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    for (file_path, line_hints) in candidates {
        if counts.len() >= max_results {
            break;
        }

        let count = if is_literal {
            count_literal_matches(index, file_path, pattern, line_hints)
        } else {
            count_regex_matches(index, file_path, matcher.as_ref().unwrap())
        };

        if count > 0 {
            if skipped < offset {
                skipped += 1;
            } else {
                counts.push(format!("{}:{}", file_path, count));
            }
        }
    }

    Ok(counts.join("\n"))
}

// ── Memory formatting helpers ──

fn format_session(s: &memory::SessionEntry) -> String {
    let date = s.started_at.as_deref().unwrap_or("unknown");
    let sid = if s.session_id.len() >= 8 {
        &s.session_id[..8]
    } else {
        &s.session_id
    };
    let msg = s
        .first_message
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(100)
        .collect::<String>();
    format!(
        "{} | {} | {} | turns:{} tools:{} | {}",
        date, s.project_dir, sid, s.turn_count, s.tool_call_count, msg
    )
}

fn format_event(e: &memory::EventEntry) -> String {
    let date = if e.event_ts.len() >= 16 {
        &e.event_ts[..16]
    } else {
        &e.event_ts
    };
    let sid = if e.session_id.len() >= 8 {
        &e.session_id[..8]
    } else {
        &e.session_id
    };
    let mk = e.manifest_key.as_deref().unwrap_or("-");
    format!(
        "{} | {} | {} | {} | {}",
        date, e.project_dir, sid, mk, e.command
    )
}

fn format_agent(a: &memory::AgentEntry) -> String {
    let aid = if a.agent_id.len() >= 8 {
        &a.agent_id[..8]
    } else {
        &a.agent_id
    };
    let pid = if a.parent_session_id.len() >= 8 {
        &a.parent_session_id[..8]
    } else {
        &a.parent_session_id
    };
    let project = a.project_dir.as_deref().unwrap_or("-");
    let msg = a
        .first_message
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(100)
        .collect::<String>();
    format!(
        "{} | parent:{} | {} | turns:{} tools:{} | {}",
        aid, pid, project, a.turn_count, a.tool_call_count, msg
    )
}

#[rmcp::tool_router]
impl NdxServer {
    #[tool(description = "List indexed files, optionally filtered by directory prefix and/or glob pattern. Supports sorting by name or modification time.")]
    async fn list_files(&self, params: Parameters<ListFilesInput>) -> Result<String, String> {
        let input = params.0;
        let sort_modified = input.sort.as_deref() == Some("modified");
        let glob_matcher = match &input.pattern {
            Some(p) => Some(Glob::new(p).map_err(|e| e.to_string())?.compile_matcher()),
            None => None,
        };

        if sort_modified {
            let mut entries = if let Some(ref prefix) = input.path {
                let prefix = if prefix.ends_with('/') {
                    prefix.clone()
                } else {
                    format!("{}/", prefix)
                };
                self.index
                    .list_prefix_with_meta(&prefix)
                    .map_err(|e| e.to_string())?
            } else {
                self.index
                    .list_all_with_meta()
                    .map_err(|e| e.to_string())?
            };

            if let Some(ref glob) = glob_matcher {
                entries.retain(|(p, _)| glob.is_match(p.as_str()));
            }

            entries.sort_by(|a, b| b.1.modified.cmp(&a.1.modified));

            Ok(entries
                .iter()
                .map(|(p, e)| format!("{}\t{}", p, format_modified(e.modified)))
                .collect::<Vec<_>>()
                .join("\n"))
        } else {
            let mut paths = if let Some(ref prefix) = input.path {
                let prefix = if prefix.ends_with('/') {
                    prefix.clone()
                } else {
                    format!("{}/", prefix)
                };
                self.index
                    .list_prefix(&prefix)
                    .map_err(|e| e.to_string())?
            } else {
                self.index.list_all().map_err(|e| e.to_string())?
            };

            if let Some(ref glob) = glob_matcher {
                paths.retain(|p| glob.is_match(p.as_str()));
            }

            Ok(paths.join("\n"))
        }
    }

    #[tool(description = "Find files matching a glob pattern. Supports sorting by name or modification time.")]
    async fn search_files(
        &self,
        params: Parameters<SearchFilesInput>,
    ) -> Result<String, String> {
        let glob = Glob::new(&params.0.pattern)
            .map_err(|e| e.to_string())?
            .compile_matcher();

        if params.0.sort.as_deref() == Some("modified") {
            let all = self
                .index
                .list_all_with_meta()
                .map_err(|e| e.to_string())?;
            let mut matched: Vec<_> = all
                .into_iter()
                .filter(|(p, _)| glob.is_match(p.as_str()))
                .collect();
            matched.sort_by(|a, b| b.1.modified.cmp(&a.1.modified));
            Ok(matched
                .iter()
                .map(|(p, e)| format!("{}\t{}", p, format_modified(e.modified)))
                .collect::<Vec<_>>()
                .join("\n"))
        } else {
            let all = self.index.list_all().map_err(|e| e.to_string())?;
            let matched: Vec<&String> =
                all.iter().filter(|p| glob.is_match(p.as_str())).collect();
            Ok(matched
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }

    #[tool(description = "Search file contents by text or regex pattern. Uses trigram index for fast candidate filtering. Supports context lines (before_context/after_context), output modes (content/files_with_matches/count), and pagination via offset.")]
    async fn search_content(
        &self,
        params: Parameters<SearchContentInput>,
    ) -> Result<String, String> {
        let index = self.index.clone();
        let pattern = params.0.pattern;
        let file_pattern = params.0.file_pattern;
        let max_results = params.0.max_results.unwrap_or(100);
        let before_ctx = params.0.before_context.unwrap_or(0);
        let after_ctx = params.0.after_context.unwrap_or(0);
        let offset = params.0.offset.unwrap_or(0);
        let output_mode = params.0.output_mode.unwrap_or_default();

        tokio::task::spawn_blocking(move || {
            let candidates =
                get_search_candidates(&index, &pattern, file_pattern.as_deref())
                    .map_err(|e| e.to_string())?;

            let is_literal = trigram::is_literal_pattern(&pattern);

            match output_mode.as_str() {
                "files_with_matches" => {
                    search_files_mode(&index, &pattern, is_literal, &candidates, offset, max_results)
                }
                "count" => {
                    search_count_mode(&index, &pattern, is_literal, &candidates, offset, max_results)
                }
                _ => search_content_mode(
                    &index,
                    &pattern,
                    is_literal,
                    &candidates,
                    offset,
                    max_results,
                    before_ctx,
                    after_ctx,
                ),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tool(description = "Show index statistics including trigram index info and memory stats")]
    async fn index_status(&self) -> Result<String, String> {
        let file_count = self.index.count().map_err(|e| e.to_string())?;
        let content_count = self
            .index
            .content_indexed_count()
            .map_err(|e| e.to_string())?;
        let trigram_count = self.index.trigram_count().map_err(|e| e.to_string())?;
        let root = self.index.root().display().to_string();
        let mut out = format!(
            "Project root: {}\nIndexed files: {}\nContent-indexed files: {}\nUnique trigrams: {}",
            root, file_count, content_count, trigram_count
        );

        if let Some(ref mem) = self.memory {
            match mem.session_stats() {
                Ok(stats) => {
                    out.push_str(&format!(
                        "\n\nMemory:\n  Sessions: {}\n  Events: {}\n  Agents: {}\n  Total turns: {}\n  Total tool calls: {}",
                        stats.session_count, stats.event_count, stats.agent_count,
                        stats.total_turns, stats.total_tool_calls
                    ));
                    if let Some(ref oldest) = stats.oldest_session {
                        out.push_str(&format!("\n  Oldest session: {}", oldest));
                    }
                    if let Some(ref newest) = stats.newest_session {
                        out.push_str(&format!("\n  Newest session: {}", newest));
                    }
                }
                Err(e) => {
                    out.push_str(&format!("\n\nMemory: error loading stats: {}", e));
                }
            }
        }

        Ok(out)
    }

    #[tool(description = "Search session memory by text query. Returns sessions whose transcripts match the query.")]
    async fn memory_search(
        &self,
        params: Parameters<MemorySearchInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let limit = params.0.limit.unwrap_or(20);
        let results = mem
            .search_sessions(&params.0.query, limit)
            .map_err(|e| e.to_string())?;
        if results.is_empty() {
            return Ok("No matching sessions found.".to_string());
        }
        Ok(results.iter().map(format_session).collect::<Vec<_>>().join("\n"))
    }

    #[tool(description = "Search event log (hooks, commands) by text query. Ingests new events first.")]
    async fn memory_events_search(
        &self,
        params: Parameters<MemoryEventsSearchInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        // Ingest new events first
        let _ = memory::event::ingest_events(mem);
        let limit = params.0.limit.unwrap_or(50);
        let results = mem
            .search_events(&params.0.query, limit)
            .map_err(|e| e.to_string())?;
        if results.is_empty() {
            return Ok("No matching events found.".to_string());
        }
        Ok(results.iter().map(format_event).collect::<Vec<_>>().join("\n"))
    }

    #[tool(description = "List recent sessions, optionally filtered by project directory.")]
    async fn memory_list(
        &self,
        params: Parameters<MemoryListInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let limit = params.0.limit.unwrap_or(20);
        let results = mem
            .list_sessions(params.0.project.as_deref(), limit)
            .map_err(|e| e.to_string())?;
        if results.is_empty() {
            return Ok("No sessions found.".to_string());
        }
        Ok(results.iter().map(format_session).collect::<Vec<_>>().join("\n"))
    }

    #[tool(description = "Show memory statistics: session/event/agent counts, top tools, date range.")]
    async fn memory_stats(
        &self,
        _params: Parameters<MemoryStatsInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let stats = mem.session_stats().map_err(|e| e.to_string())?;
        let mut out = format!(
            "Sessions: {}\nEvents: {}\nAgents: {}\nTotal turns: {}\nTotal tool calls: {}",
            stats.session_count, stats.event_count, stats.agent_count,
            stats.total_turns, stats.total_tool_calls
        );
        if let Some(ref oldest) = stats.oldest_session {
            out.push_str(&format!("\nOldest session: {}", oldest));
        }
        if let Some(ref newest) = stats.newest_session {
            out.push_str(&format!("\nNewest session: {}", newest));
        }
        if !stats.top_tools.is_empty() {
            out.push_str("\n\nTop tools:");
            for (tool, count) in &stats.top_tools {
                out.push_str(&format!("\n  {}: {}", tool, count));
            }
        }
        Ok(out)
    }

    #[tool(description = "Get full details for a specific session by ID.")]
    async fn memory_session_detail(
        &self,
        params: Parameters<MemorySessionDetailInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let session = mem
            .get_session(&params.0.session_id)
            .map_err(|e| e.to_string())?
            .ok_or("session not found")?;
        let mut out = format!(
            "Session: {}\nProject: {}\nSlug: {}\nBranch: {}\nModel: {}\nStarted: {}\nEnded: {}\nTurns: {}\nTool calls: {}\nTools: {}\nFiles: {}\nFirst message: {}",
            session.session_id,
            session.project_dir,
            session.slug,
            session.git_branch.as_deref().unwrap_or("-"),
            session.model.as_deref().unwrap_or("-"),
            session.started_at.as_deref().unwrap_or("-"),
            session.ended_at.as_deref().unwrap_or("-"),
            session.turn_count,
            session.tool_call_count,
            session.tool_names.join(", "),
            session.files.len(),
            session.first_message.as_deref().unwrap_or("-"),
        );
        if !session.files.is_empty() {
            out.push_str("\n\nFiles:");
            for f in &session.files {
                out.push_str(&format!("\n  {}", f));
            }
        }
        Ok(out)
    }

    #[tool(description = "Get recent context for a project: last 5 sessions + last 20 events.")]
    async fn memory_project_context(
        &self,
        params: Parameters<MemoryProjectContextInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let project = params.0.project.as_deref();
        let sessions = mem
            .list_sessions(project, 5)
            .map_err(|e| e.to_string())?;
        let events = mem
            .list_events(project, 20)
            .map_err(|e| e.to_string())?;

        let mut out = String::new();
        out.push_str("Recent sessions:");
        if sessions.is_empty() {
            out.push_str("\n  (none)");
        } else {
            for s in &sessions {
                out.push_str(&format!("\n  {}", format_session(s)));
            }
        }
        out.push_str("\n\nRecent events:");
        if events.is_empty() {
            out.push_str("\n  (none)");
        } else {
            for e in &events {
                out.push_str(&format!("\n  {}", format_event(e)));
            }
        }
        Ok(out)
    }

    #[tool(description = "Search subagent transcripts by text query. Scans for new agents first.")]
    async fn memory_subagent_search(
        &self,
        params: Parameters<MemorySubagentSearchInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        // Scan for new agents first
        let _ = memory::agent::scan_agents(mem);
        let limit = params.0.limit.unwrap_or(20);
        let results = mem
            .search_agents(&params.0.query, params.0.parent_session_id.as_deref(), limit)
            .map_err(|e| e.to_string())?;
        if results.is_empty() {
            return Ok("No matching subagents found.".to_string());
        }
        Ok(results.iter().map(format_agent).collect::<Vec<_>>().join("\n"))
    }

    #[tool(description = "Show a session and its subagent tree.")]
    async fn memory_session_tree(
        &self,
        params: Parameters<MemorySessionTreeInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let session = mem
            .get_session(&params.0.session_id)
            .map_err(|e| e.to_string())?
            .ok_or("session not found")?;
        let agents = mem
            .list_agents_by_parent(&params.0.session_id)
            .map_err(|e| e.to_string())?;

        let mut out = format!("Session: {}", format_session(&session));
        if agents.is_empty() {
            out.push_str("\n\nNo subagents.");
        } else {
            out.push_str(&format!("\n\nSubagents ({}):", agents.len()));
            for a in &agents {
                out.push_str(&format!("\n  {}", format_agent(a)));
            }
        }
        Ok(out)
    }

    #[tool(description = "Find sessions that touched a given file path.")]
    async fn file_sessions(
        &self,
        params: Parameters<FileSessionsInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let limit = params.0.limit.unwrap_or(10);
        let results = mem
            .sessions_for_file(&params.0.path, limit)
            .map_err(|e| e.to_string())?;
        if results.is_empty() {
            return Ok("No sessions found for this file.".to_string());
        }
        Ok(results.iter().map(format_session).collect::<Vec<_>>().join("\n"))
    }

    #[tool(description = "List files touched by a session, with current status (exists/deleted).")]
    async fn session_files(
        &self,
        params: Parameters<SessionFilesInput>,
    ) -> Result<String, String> {
        let mem = self.memory.as_ref().ok_or("memory not available")?;
        let files = memory::xref::files_for_session_with_status(mem, &self.index, &params.0.session_id)
            .map_err(|e| e.to_string())?;
        if files.is_empty() {
            return Ok("No files recorded for this session.".to_string());
        }
        let mut out = Vec::new();
        for f in &files {
            let size_str = f.size.map(|s| format!(" {}B", s)).unwrap_or_default();
            let mod_str = f.modified.as_deref().unwrap_or("");
            out.push(format!("{} [{}]{} {}", f.path, f.status, size_str, mod_str));
        }
        Ok(out.join("\n"))
    }
}

#[rmcp::tool_handler]
impl ServerHandler for NdxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities {
                tools: Some(Default::default()),
                ..Default::default()
            },
            server_info: Implementation {
                name: "ndx".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(
                "File index server with fast file listing, content search (trigram index), session memory, event hooks, and subagent tracking."
                    .to_string(),
            ),
        }
    }
}
