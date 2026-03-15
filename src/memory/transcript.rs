use super::{AgentEntry, SessionEntry};
use anyhow::Result;
use std::path::Path;

const MAX_FIRST_MSG: usize = 500;
const MAX_ALL_USER: usize = 8000;

struct Accumulator {
    session_id: String,
    project_dir: Option<String>,
    git_branch: Option<String>,
    slug: String,
    model: Option<String>,
    started_at: Option<String>,
    ended_at: Option<String>,
    turn_count: u32,
    tool_call_count: u32,
    tool_names: LinkedHashSet<String>,
    files: LinkedHashSet<String>,
    first_message: Option<String>,
    all_user_text: String,
}

impl Accumulator {
    fn new(session_id: String, slug: String) -> Self {
        Self {
            session_id,
            project_dir: None,
            git_branch: None,
            slug,
            model: None,
            started_at: None,
            ended_at: None,
            turn_count: 0,
            tool_call_count: 0,
            tool_names: LinkedHashSet::new(),
            files: LinkedHashSet::new(),
            first_message: None,
            all_user_text: String::new(),
        }
    }

    fn capture_timestamp(&mut self, ts: Option<&str>) {
        if let Some(t) = ts {
            if !t.is_empty() {
                if self.started_at.is_none() {
                    self.started_at = Some(t.to_string());
                }
                self.ended_at = Some(t.to_string());
            }
        }
    }

    fn add_user_text(&mut self, text: Option<&str>) {
        if let Some(t) = text {
            if t.is_empty() {
                return;
            }
            if self.first_message.is_none() {
                self.first_message = Some(if t.len() > MAX_FIRST_MSG {
                    t[..MAX_FIRST_MSG].to_string()
                } else {
                    t.to_string()
                });
            }
            if self.all_user_text.len() < MAX_ALL_USER {
                if !self.all_user_text.is_empty() {
                    self.all_user_text.push(' ');
                }
                self.all_user_text.push_str(t);
            }
        }
    }

    fn add_tool(&mut self, name: &str, tool_input: Option<&serde_json::Value>) {
        self.tool_names.insert(name.to_string());
        self.tool_call_count += 1;
        if let Some(input) = tool_input {
            Self::extract_file_paths(input, &mut self.files);
        }
    }

    fn extract_file_paths(node: &serde_json::Value, files: &mut LinkedHashSet<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(p)) = map.get("file_path") {
                    files.insert(p.clone());
                }
                if let Some(serde_json::Value::String(p)) = map.get("path") {
                    files.insert(p.clone());
                }
                for v in map.values() {
                    Self::extract_file_paths(v, files);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    Self::extract_file_paths(v, files);
                }
            }
            _ => {}
        }
    }
}

fn default_session_id(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

fn decode_slug(slug: &str) -> String {
    slug.replace('-', "/")
}

fn json_text<'a>(node: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    node.get(field).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
}

fn extract_text_blocks(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut sb = String::new();
            for block in arr {
                if let serde_json::Value::String(s) = block {
                    if !sb.is_empty() {
                        sb.push(' ');
                    }
                    sb.push_str(s);
                } else if let Some(text) = json_text(block, "text") {
                    if !sb.is_empty() {
                        sb.push(' ');
                    }
                    sb.push_str(text);
                }
            }
            if sb.is_empty() { None } else { Some(sb) }
        }
        _ => None,
    }
}

pub fn parse_claude_session(path: &Path, slug: &str) -> Result<Option<SessionEntry>> {
    let body = std::fs::read_to_string(path)?;
    if body.trim().is_empty() {
        return Ok(None);
    }

    let mut acc = Accumulator::new(default_session_id(path), slug.to_string());

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let node: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if acc.project_dir.is_none() {
            if let Some(cwd) = json_text(&node, "cwd") {
                acc.project_dir = Some(cwd.to_string());
            }
        }
        if let Some(sid) = json_text(&node, "sessionId") {
            acc.session_id = sid.to_string();
        }
        if acc.git_branch.is_none() {
            if let Some(branch) = json_text(&node, "gitBranch") {
                acc.git_branch = Some(branch.to_string());
            }
        }

        let ts = json_text(&node, "timestamp");
        acc.capture_timestamp(ts);

        match json_text(&node, "type") {
            Some("human") => {
                let text = node
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| extract_text_blocks(c));
                acc.add_user_text(text.as_deref());
            }
            Some("assistant") => {
                let message = node.get("message");
                if acc.model.is_none() {
                    if let Some(m) = message.and_then(|m| json_text(m, "model")) {
                        acc.model = Some(m.to_string());
                    }
                }
                if let Some(content) = message.and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                    for block in content {
                        if json_text(block, "type") == Some("tool_use") {
                            let name = json_text(block, "name").unwrap_or("unknown");
                            acc.add_tool(name, block.get("input"));
                        }
                    }
                }
                acc.turn_count += 1;
            }
            _ => {}
        }
    }

    if acc.project_dir.is_none() {
        if let Some(parent) = path.parent().and_then(|p| p.file_name()) {
            acc.project_dir = Some(decode_slug(&parent.to_string_lossy()));
        }
    }

    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(Some(SessionEntry {
        session_id: acc.session_id,
        project_dir: acc.project_dir.unwrap_or_default(),
        git_branch: acc.git_branch,
        slug: acc.slug,
        model: acc.model,
        started_at: acc.started_at,
        ended_at: acc.ended_at,
        turn_count: acc.turn_count,
        tool_call_count: acc.tool_call_count,
        tool_names: acc.tool_names.into_iter().collect(),
        files: acc.files.into_iter().collect(),
        first_message: acc.first_message,
        all_user_text: if acc.all_user_text.len() > MAX_ALL_USER {
            acc.all_user_text[..MAX_ALL_USER].to_string()
        } else {
            acc.all_user_text
        },
        scanned_at: chrono::Utc::now().to_rfc3339(),
        source_path: path.to_string_lossy().into_owned(),
        source_modified: mtime,
    }))
}

pub fn parse_agent_session(path: &Path, parent_session_id: &str) -> Result<Option<AgentEntry>> {
    let body = std::fs::read_to_string(path)?;
    if body.trim().is_empty() {
        return Ok(None);
    }

    let agent_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .strip_prefix("agent-")
        .unwrap_or("")
        .to_string();

    if agent_id.is_empty() {
        return Ok(None);
    }

    let mut acc = Accumulator::new(agent_id.clone(), String::new());
    let mut message_count: u32 = 0;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let node: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        message_count += 1;

        if acc.project_dir.is_none() {
            if let Some(cwd) = json_text(&node, "cwd") {
                acc.project_dir = Some(cwd.to_string());
            }
        }
        if acc.model.is_none() {
            if let Some(m) = node.get("message").and_then(|m| json_text(m, "model")) {
                acc.model = Some(m.to_string());
            }
        }

        let ts = json_text(&node, "timestamp");
        acc.capture_timestamp(ts);

        match json_text(&node, "type") {
            Some("human") => {
                let text = node
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| extract_text_blocks(c));
                acc.add_user_text(text.as_deref());
            }
            Some("assistant") => {
                if let Some(content) = node
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if json_text(block, "type") == Some("tool_use") {
                            let name = json_text(block, "name").unwrap_or("unknown");
                            acc.add_tool(name, block.get("input"));
                        }
                    }
                }
                acc.turn_count += 1;
            }
            _ => {}
        }
    }

    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(Some(AgentEntry {
        agent_id,
        parent_session_id: parent_session_id.to_string(),
        agent_slug: if acc.slug.is_empty() { None } else { Some(acc.slug) },
        project_dir: acc.project_dir,
        cwd: None,
        model: acc.model,
        turn_count: acc.turn_count,
        tool_call_count: acc.tool_call_count,
        tool_names: acc.tool_names.into_iter().collect(),
        first_message: acc.first_message,
        all_user_text: if acc.all_user_text.len() > MAX_ALL_USER {
            acc.all_user_text[..MAX_ALL_USER].to_string()
        } else {
            acc.all_user_text
        },
        first_seen_at: acc.started_at,
        last_updated_at: acc.ended_at,
        message_count,
        scanned_at: chrono::Utc::now().to_rfc3339(),
        source_path: path.to_string_lossy().into_owned(),
        source_modified: mtime,
    }))
}

// LinkedHashSet for preserving insertion order
use std::collections::HashSet;
use std::hash::Hash;

#[derive(Debug, Clone)]
struct LinkedHashSet<T: Eq + Hash> {
    set: HashSet<T>,
    order: Vec<T>,
}

impl<T: Eq + Hash + Clone> LinkedHashSet<T> {
    fn new() -> Self {
        Self {
            set: HashSet::new(),
            order: Vec::new(),
        }
    }

    fn insert(&mut self, value: T) -> bool {
        if self.set.insert(value.clone()) {
            self.order.push(value);
            true
        } else {
            false
        }
    }
}

impl<T: Eq + Hash + Clone> IntoIterator for LinkedHashSet<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.order.into_iter()
    }
}
