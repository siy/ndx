pub struct ParsedCommand {
    pub key: String,
    pub cmd: String,
    pub subcommand: Option<String>,
}

pub fn parse_command(shell_command: &str) -> Option<ParsedCommand> {
    // Skip subshells and backtick expressions
    if shell_command.contains("$(") || shell_command.contains('`') {
        return None;
    }

    // Take first pipeline segment
    let first_segment = shell_command
        .split(&['|', '&', ';'][..])
        .next()?
        .trim();

    // Strip leading env var assignments and sudo
    let stripped = strip_env_and_sudo(first_segment);

    // Split on whitespace
    let parts: Vec<&str> = stripped.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    // Extract bare command name (strip path)
    let cmd = parts[0]
        .rsplit('/')
        .next()
        .unwrap_or(parts[0])
        .to_string();

    if cmd.is_empty() {
        return None;
    }

    // Compound key: if second arg looks like a subcommand word
    if parts.len() > 1 && is_subcommand_word(parts[1]) {
        let sub = parts[1].to_string();
        return Some(ParsedCommand {
            key: format!("{}-{}", cmd, sub),
            cmd,
            subcommand: Some(sub),
        });
    }

    Some(ParsedCommand {
        key: cmd.clone(),
        cmd,
        subcommand: None,
    })
}

fn strip_env_and_sudo(s: &str) -> &str {
    let mut rest = s;

    // Strip env var assignments: KEY=VALUE
    loop {
        let trimmed = rest.trim_start();
        if let Some(pos) = trimmed.find(|c: char| c.is_whitespace()) {
            let word = &trimmed[..pos];
            if word.contains('=') && !word.starts_with('-') {
                rest = &trimmed[pos..];
                continue;
            }
        }
        break;
    }

    // Strip sudo
    let trimmed = rest.trim_start();
    if trimmed.starts_with("sudo ") || trimmed.starts_with("sudo\t") {
        return trimmed[5..].trim_start();
    }

    trimmed
}

fn is_subcommand_word(word: &str) -> bool {
    if word.is_empty() || word.starts_with('-') || word.starts_with('/') || word.contains('.') {
        return false;
    }

    let first = word.chars().next().unwrap();
    if !first.is_ascii_lowercase() {
        return false;
    }

    word.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compound_command() {
        // "ps aux" → compound key "ps-aux", fallback to "ps" in manifest lookup
        let p = parse_command("ps aux").unwrap();
        assert_eq!(p.key, "ps-aux");
        assert_eq!(p.cmd, "ps");
        assert_eq!(p.subcommand.as_deref(), Some("aux"));
    }

    #[test]
    fn test_git_subcommand() {
        let p = parse_command("git log --oneline").unwrap();
        assert_eq!(p.key, "git-log");
        assert_eq!(p.cmd, "git");
        assert_eq!(p.subcommand.as_deref(), Some("log"));
    }

    #[test]
    fn test_sudo() {
        let p = parse_command("sudo docker ps").unwrap();
        assert_eq!(p.key, "docker-ps");
    }

    #[test]
    fn test_env_var() {
        let p = parse_command("RUST_LOG=debug cargo test").unwrap();
        assert_eq!(p.key, "cargo-test");
    }

    #[test]
    fn test_pipeline() {
        // Pipeline: only first segment parsed, "ps aux" → compound "ps-aux"
        let p = parse_command("ps aux | grep foo").unwrap();
        assert_eq!(p.cmd, "ps");
    }

    #[test]
    fn test_subshell_rejected() {
        assert!(parse_command("echo $(date)").is_none());
    }

    #[test]
    fn test_flag_not_subcommand() {
        let p = parse_command("ls -la").unwrap();
        assert_eq!(p.key, "ls");
        assert!(p.subcommand.is_none());
    }
}
