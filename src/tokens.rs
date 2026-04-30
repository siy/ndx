//! Per-file token cost estimation for the file index.
//!
//! Helps Claude pick the cheaper file when several would do. Estimates
//! are deliberately rough — `chars / ratio_for_extension` — not real
//! tokenization. Zero dependencies, zero stored state: ratios are a
//! pure function of file extension; the size already lives in
//! `FileEntry::size`.
//!
//! Ratios calibrated against typical content shapes:
//! - Prose / Markdown tokenizes near the English-language baseline.
//! - Source code tokenizes denser (more punctuation, shorter identifiers).
//! - JSON / YAML / TOML / XML are whitespace-heavy.
//!
//! When a file's extension is unrecognized, the conservative default
//! (3.5) splits the difference between code and prose.

/// Approximate tokens for `size_bytes` bytes of content at `path`.
/// Returns 0 for empty files. The estimate rounds half away from zero.
pub fn estimate_tokens(path: &str, size_bytes: u64) -> u64 {
    if size_bytes == 0 {
        return 0;
    }
    let ratio = ratio_for_path(path);
    ((size_bytes as f64) / ratio).round() as u64
}

fn ratio_for_path(path: &str) -> f64 {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        // Prose / docs — close to English baseline.
        "md" | "txt" | "rst" | "adoc" => 3.8,

        // Source code — denser tokenization.
        "rs" | "py" | "go" | "ts" | "tsx" | "js" | "jsx" | "java" | "kt" | "swift"
        | "c" | "cpp" | "cc" | "h" | "hpp" | "cs" | "rb" | "scala" | "sh" | "bash" => 3.0,

        // Whitespace / structure heavy.
        "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "htm" | "css" | "svg" => 4.5,

        // Default — splits the difference.
        _ => 3.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_zero_tokens() {
        assert_eq!(estimate_tokens("src/main.rs", 0), 0);
    }

    #[test]
    fn rust_file_uses_3_0_ratio() {
        // 3000 bytes / 3.0 = 1000 tokens
        assert_eq!(estimate_tokens("src/main.rs", 3000), 1000);
    }

    #[test]
    fn markdown_uses_3_8_ratio() {
        // 3800 / 3.8 = 1000
        assert_eq!(estimate_tokens("README.md", 3800), 1000);
    }

    #[test]
    fn json_uses_4_5_ratio() {
        // 4500 / 4.5 = 1000
        assert_eq!(estimate_tokens("config.json", 4500), 1000);
    }

    #[test]
    fn unknown_extension_uses_3_5_default() {
        // 3500 / 3.5 = 1000
        assert_eq!(estimate_tokens("data.parquet", 3500), 1000);
    }

    #[test]
    fn no_extension_uses_default() {
        assert_eq!(estimate_tokens("Makefile", 3500), 1000);
    }

    #[test]
    fn extension_lookup_is_case_insensitive() {
        assert_eq!(
            estimate_tokens("Foo.RS", 3000),
            estimate_tokens("foo.rs", 3000)
        );
    }

    #[test]
    fn rounds_half_away_from_zero() {
        // 100 / 3.0 = 33.333… → 33
        assert_eq!(estimate_tokens("a.rs", 100), 33);
        // 5 / 3.0 = 1.666… → 2
        assert_eq!(estimate_tokens("a.rs", 5), 2);
    }
}
