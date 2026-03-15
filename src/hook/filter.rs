use super::manifest;
use regex::Regex;
use std::io::{self, BufRead, Write};

pub fn is_filterable(command: &str) -> bool {
    !command.contains('>') && !command.contains('<') && !command.contains("exec")
}

pub fn run_filter(key: &str) -> io::Result<()> {
    let manifest = match manifest::resolve_manifest(key, None) {
        Some(m) => m,
        None => {
            return passthrough();
        }
    };

    let output_schema = match manifest.output_schema {
        Some(ref os) if os.enable_filter => os,
        _ => return passthrough(),
    };

    let noise_regexes: Vec<Regex> = output_schema
        .noise_patterns
        .iter()
        .filter_map(|np| Regex::new(&np.pattern).ok())
        .collect();

    let max_lines = output_schema.max_lines as usize;
    let truncation_msg = output_schema
        .truncation_message
        .as_deref()
        .unwrap_or("... {remaining} more lines truncated.");

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let mut buffered_lines: Vec<String> = Vec::new();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        // Skip blank lines
        if line.trim().is_empty() {
            continue;
        }

        // Check noise patterns
        if noise_regexes.iter().any(|re| re.is_match(&line)) {
            continue;
        }

        buffered_lines.push(line);
    }

    // Apply max_lines truncation
    if max_lines > 0 && buffered_lines.len() > max_lines {
        let remaining = buffered_lines.len() - max_lines;
        for line in &buffered_lines[..max_lines] {
            writeln!(out, "{}", line)?;
        }
        let msg = truncation_msg.replace("{remaining}", &remaining.to_string());
        writeln!(out, "{}", msg)?;
    } else {
        for line in &buffered_lines {
            writeln!(out, "{}", line)?;
        }
    }

    out.flush()?;
    Ok(())
}

fn passthrough() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        if let Ok(line) = line {
            writeln!(out, "{}", line)?;
        }
    }
    out.flush()?;
    Ok(())
}
