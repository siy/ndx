use crate::index::Index;
use crate::trigram;
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
    tool_router: ToolRouter<Self>,
}

impl NdxServer {
    pub fn new(index: Arc<Index>) -> Self {
        Self {
            index,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ListFilesInput {
    /// Optional directory prefix to filter by
    pub path: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchFilesInput {
    /// Glob pattern to match file paths
    pub pattern: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchContentInput {
    /// Text or regex pattern to search for
    pub pattern: String,
    /// Optional glob to filter which files to search
    pub file_pattern: Option<String>,
    /// Maximum number of results (default: 100)
    pub max_results: Option<usize>,
}

/// Get candidate files with line hints for content search using trigram index.
fn get_search_candidates(
    index: &Index,
    pattern: &str,
    file_pattern: Option<&str>,
) -> Result<Vec<(String, Vec<u32>)>, anyhow::Error> {
    // Try to extract a literal from the pattern for trigram lookup
    let results = match trigram::extract_longest_literal(pattern) {
        Some(literal) => match index.search_trigram(literal)? {
            Some(candidates) => candidates,
            None => all_files_no_lines(index)?, // literal too short for trigrams
        },
        None => {
            // No literals in regex — try the raw pattern itself
            // (works when pattern is a plain string with no metacharacters)
            match index.search_trigram(pattern)? {
                Some(candidates) => candidates,
                None => all_files_no_lines(index)?,
            }
        }
    };

    if let Some(fp) = file_pattern {
        let glob = Glob::new(fp)?.compile_matcher();
        Ok(results
            .into_iter()
            .filter(|(p, _)| glob.is_match(p.as_str()))
            .collect())
    } else {
        Ok(results)
    }
}

fn all_files_no_lines(index: &Index) -> Result<Vec<(String, Vec<u32>)>, anyhow::Error> {
    Ok(index
        .list_all()?
        .into_iter()
        .map(|p| (p, Vec::new()))
        .collect())
}

#[rmcp::tool_router]
impl NdxServer {
    #[tool(description = "List indexed files, optionally filtered by directory prefix")]
    async fn list_files(
        &self,
        params: Parameters<ListFilesInput>,
    ) -> Result<String, String> {
        let paths = if let Some(ref prefix) = params.0.path {
            let prefix = if prefix.ends_with('/') {
                prefix.clone()
            } else {
                format!("{}/", prefix)
            };
            self.index.list_prefix(&prefix).map_err(|e| e.to_string())?
        } else {
            self.index.list_all().map_err(|e| e.to_string())?
        };
        Ok(paths.join("\n"))
    }

    #[tool(description = "Find files matching a glob pattern")]
    async fn search_files(
        &self,
        params: Parameters<SearchFilesInput>,
    ) -> Result<String, String> {
        let glob = Glob::new(&params.0.pattern)
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let all = self.index.list_all().map_err(|e| e.to_string())?;
        let matched: Vec<&String> = all.iter().filter(|p| glob.is_match(p.as_str())).collect();
        Ok(matched.into_iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    #[tool(description = "Search file contents by text or regex pattern. Uses trigram index for fast candidate filtering with line-level positions for literals, full regex match for patterns.")]
    async fn search_content(
        &self,
        params: Parameters<SearchContentInput>,
    ) -> Result<String, String> {
        let index = self.index.clone();
        let pattern = params.0.pattern;
        let file_pattern = params.0.file_pattern;
        let max_results = params.0.max_results.unwrap_or(100);

        tokio::task::spawn_blocking(move || {
            let candidates = get_search_candidates(&index, &pattern, file_pattern.as_deref())
                .map_err(|e| e.to_string())?;

            let mut results: Vec<String> = Vec::new();

            if trigram::is_literal_pattern(&pattern) {
                // Line-level matching for literal patterns
                for (file_path, line_nums) in &candidates {
                    if results.len() >= max_results {
                        break;
                    }
                    let abs_path = index.abs_path(file_path);
                    let content = match std::fs::read_to_string(&abs_path) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let lines: Vec<&str> = content.lines().collect();

                    if line_nums.is_empty() {
                        // No line hints (pattern too short for trigrams), scan all lines
                        for (idx, line) in lines.iter().enumerate() {
                            if results.len() >= max_results {
                                break;
                            }
                            if line.contains(pattern.as_str()) {
                                results.push(format!(
                                    "{}:{}:{}",
                                    file_path,
                                    idx + 1,
                                    line.trim_end()
                                ));
                            }
                        }
                    } else {
                        // Check only candidate lines from trigram index
                        for &line_num in line_nums {
                            if results.len() >= max_results {
                                break;
                            }
                            let idx = (line_num as usize).saturating_sub(1);
                            if idx < lines.len() && lines[idx].contains(pattern.as_str()) {
                                results.push(format!(
                                    "{}:{}:{}",
                                    file_path,
                                    line_num,
                                    lines[idx].trim_end()
                                ));
                            }
                        }
                    }
                }
            } else {
                // Regex matching — use grep on candidate files
                let matcher = RegexMatcher::new(&pattern).map_err(|e| e.to_string())?;
                let mut searcher = Searcher::new();

                for (file_path, _) in &candidates {
                    if results.len() >= max_results {
                        break;
                    }
                    let abs_path = index.abs_path(file_path);
                    let fp = file_path.as_str();
                    let remaining = max_results - results.len();
                    let mut count = 0usize;
                    let _ = searcher.search_path(
                        &matcher,
                        &abs_path,
                        UTF8(|lnum, line| {
                            results.push(format!("{}:{}:{}", fp, lnum, line.trim_end()));
                            count += 1;
                            Ok(count < remaining)
                        }),
                    );
                }
            }

            Ok(results.join("\n"))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    #[tool(description = "Show index statistics including trigram index info")]
    async fn index_status(&self) -> Result<String, String> {
        let file_count = self.index.count().map_err(|e| e.to_string())?;
        let content_count = self.index.content_indexed_count().map_err(|e| e.to_string())?;
        let trigram_count = self.index.trigram_count().map_err(|e| e.to_string())?;
        let root = self.index.root().display().to_string();
        Ok(format!(
            "Project root: {}\nIndexed files: {}\nContent-indexed files: {}\nUnique trigrams: {}",
            root, file_count, content_count, trigram_count
        ))
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
                "File index server providing fast file listing and content search with trigram index.".to_string(),
            ),
        }
    }
}
