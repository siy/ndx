use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct CommandManifest {
    pub command: String,
    #[serde(default)]
    pub subcommand: Option<String>,
    #[serde(default = "default_platform")]
    pub platform: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub generated: Option<bool>,
    #[serde(default)]
    pub syntax: Option<SyntaxBlock>,
    #[serde(default)]
    pub output_schema: Option<OutputSchema>,
}

fn default_platform() -> String {
    "all".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct SyntaxBlock {
    #[serde(default)]
    pub usage: Option<String>,
    #[serde(default)]
    pub key_flags: Vec<KeyFlag>,
    #[serde(default)]
    pub preferred_invocations: Vec<PreferredInvocation>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KeyFlag {
    pub flag: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub use_when: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PreferredInvocation {
    pub invocation: String,
    #[serde(default)]
    pub use_when: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputSchema {
    #[serde(default)]
    pub enable_filter: bool,
    #[serde(default)]
    pub noise_patterns: Vec<NoisePattern>,
    #[serde(default)]
    pub max_lines: u32,
    #[serde(default)]
    pub truncation_message: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NoisePattern {
    pub pattern: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Three-tier manifest lookup:
/// 1. .kcp/commands/<key>.yaml (project-local)
/// 2. ~/.kcp/commands/<key>.yaml (user-level)
/// 3. ~/.ndx/commands/<key>.yaml (bundled/downloaded)
pub fn resolve_manifest(key: &str, cwd: Option<&str>) -> Option<CommandManifest> {
    let filename = format!("{}.yaml", key);

    // Tier 1: project-local
    if let Some(cwd) = cwd {
        let path = Path::new(cwd).join(".kcp").join("commands").join(&filename);
        if let Some(m) = load_manifest(&path) {
            return Some(m);
        }
    }

    // Tier 2: user-level (~/.kcp/commands/)
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".kcp").join("commands").join(&filename);
        if let Some(m) = load_manifest(&path) {
            return Some(m);
        }
    }

    // Tier 3: bundled (~/.ndx/commands/)
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".ndx").join("commands").join(&filename);
        if let Some(m) = load_manifest(&path) {
            return Some(m);
        }
    }

    None
}

fn load_manifest(path: &Path) -> Option<CommandManifest> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&content).ok()
}

pub fn platform_matches(platform: &str) -> bool {
    match platform {
        "all" => true,
        "linux" => cfg!(target_os = "linux"),
        "macos" | "darwin" => cfg!(target_os = "macos"),
        "windows" => cfg!(target_os = "windows"),
        _ => true,
    }
}

pub fn manifest_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".ndx").join("commands"));
        dirs.push(home.join(".kcp").join("commands"));
    }
    dirs
}
