//! Identity TOML parser, deep-merge, and L0 rendering.
//!
//! Implements spec §7 (R-301..R-323). Global identity lives at
//! `~/.ndx/identity.toml`; per-project override at `{project}/.ndx/identity.toml`
//! merges on top (tables recursively, arrays wholesale, scalars override).

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use toml::Value;

/// Resolve the global identity path (`~/.ndx/identity.toml`). Does not check
/// for existence.
pub fn global_identity_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".ndx").join("identity.toml"))
}

/// Resolve the per-project identity path (`{project}/.ndx/identity.toml`).
pub fn project_identity_path(project_root: &Path) -> PathBuf {
    project_root.join(".ndx").join("identity.toml")
}

/// Load, parse, and deep-merge the global + per-project identity files.
/// Returns `None` if neither file exists. Syntax errors are reported via
/// `Err` so callers can decide whether to surface them or degrade to an L0
/// error marker (R-304).
pub fn load_merged(project_root: &Path) -> Result<Option<Value>> {
    let global = global_identity_path()?;
    let project = project_identity_path(project_root);

    let global_val = read_toml_if_exists(&global)?;
    let project_val = read_toml_if_exists(&project)?;

    match (global_val, project_val) {
        (None, None) => Ok(None),
        (Some(g), None) => Ok(Some(g)),
        (None, Some(p)) => Ok(Some(p)),
        (Some(g), Some(p)) => Ok(Some(merge(g, p))),
    }
}

fn read_toml_if_exists(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: Value = toml::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(parsed))
}

/// Deep-merge `override_val` onto `base`. Rules (R-312):
///   - Tables merge recursively
///   - Arrays replace wholesale (no concat)
///   - Scalars override
pub fn merge(base: Value, override_val: Value) -> Value {
    match (base, override_val) {
        (Value::Table(mut base_tbl), Value::Table(over_tbl)) => {
            for (k, v) in over_tbl {
                match base_tbl.remove(&k) {
                    Some(existing) => {
                        base_tbl.insert(k, merge(existing, v));
                    }
                    None => {
                        base_tbl.insert(k, v);
                    }
                }
            }
            Value::Table(base_tbl)
        }
        // Arrays and scalars: override wins.
        (_, over) => over,
    }
}

/// Render the merged identity to L0 output (R-321..R-323).
///
/// The `project_name` argument (when provided) is used to select which
/// `[projects.<name>]` section is rendered in full; others collapse to a
/// one-line summary.
pub fn render_l0(merged: Option<&Value>, project_name: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("## L0 — IDENTITY\n");

    let tbl = match merged {
        Some(Value::Table(t)) => t,
        Some(_) => {
            out.push_str("*(identity file is not a TOML table)*\n");
            return out;
        }
        None => {
            out.push_str(
                "*(no identity configured — create `~/.ndx/identity.toml` or run \
                 `ndx recall identity edit`)*\n",
            );
            return out;
        }
    };

    // Fixed render order per R-322.
    if let Some(Value::String(name)) = tbl.get("name") {
        out.push_str(&format!("Name: {}\n", name));
    }
    if let Some(Value::String(role)) = tbl.get("role") {
        out.push_str(&format!("Role: {}\n", role));
    }

    if let Some(Value::Table(traits)) = tbl.get("traits") {
        if !traits.is_empty() {
            out.push_str("\nTraits:\n");
            let mut keys: Vec<&String> = traits.keys().collect();
            keys.sort();
            for k in keys {
                if let Some(v) = traits.get(k) {
                    out.push_str(&format!("  - {}: {}\n", k, render_scalar(v)));
                }
            }
        }
    }

    if let Some(Value::Table(people)) = tbl.get("people") {
        if !people.is_empty() {
            out.push_str("\nPeople:\n");
            let mut keys: Vec<&String> = people.keys().collect();
            keys.sort();
            for k in keys {
                if let Some(Value::Table(person)) = people.get(k) {
                    let relation = person
                        .get("relation")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let context = person
                        .get("context")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    out.push_str(&format!(
                        "  - {}{}{}{}\n",
                        k,
                        if !relation.is_empty() { " — " } else { "" },
                        relation,
                        if !context.is_empty() {
                            format!(" ({})", context)
                        } else {
                            String::new()
                        }
                    ));
                }
            }
        }
    }

    if let Some(Value::Table(projects)) = tbl.get("projects") {
        render_projects_section(&mut out, projects, project_name);
    }

    if let Some(Value::String(notes)) = tbl.get("notes") {
        if !notes.trim().is_empty() {
            out.push_str("\nNotes:\n");
            for line in notes.trim().lines() {
                out.push_str(&format!("  {}\n", line));
            }
        }
    }

    // Unknown top-level fields (R-304 lenient handling).
    let known: [&str; 5] = ["name", "role", "traits", "people", "projects"];
    let unknown: Vec<&String> = tbl
        .keys()
        .filter(|k| !known.contains(&k.as_str()) && k.as_str() != "notes")
        .collect();
    if !unknown.is_empty() {
        out.push_str("\nMiscellaneous:\n");
        let mut unknown = unknown;
        unknown.sort();
        for k in unknown {
            if let Some(v) = tbl.get(k) {
                out.push_str(&format!("  - {}: {}\n", k, render_scalar(v)));
            }
        }
    }

    out
}

fn render_projects_section(
    out: &mut String,
    projects: &toml::map::Map<String, Value>,
    current: Option<&str>,
) {
    if projects.is_empty() {
        return;
    }
    let mut keys: Vec<&String> = projects.keys().collect();
    keys.sort();

    // Full render for the current project (R-323).
    if let Some(cur) = current {
        if let Some(Value::Table(proj)) = projects.get(cur) {
            out.push_str(&format!("\nProject: {}\n", cur));
            let mut sub_keys: Vec<&String> = proj.keys().collect();
            sub_keys.sort();
            for k in sub_keys {
                if let Some(v) = proj.get(k) {
                    out.push_str(&format!("  {}: {}\n", k, render_scalar(v)));
                }
            }
        }
    }

    // One-line summary of other projects.
    let others: Vec<&String> = keys
        .into_iter()
        .filter(|k| Some(k.as_str()) != current)
        .collect();
    if !others.is_empty() {
        let names: Vec<&str> = others.iter().map(|s| s.as_str()).collect();
        out.push_str(&format!("Other projects: {}\n", names.join(", ")));
    }
}

fn render_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Datetime(dt) => dt.to_string(),
        Value::Array(a) => {
            let items: Vec<String> = a.iter().map(render_scalar).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Table(t) => format!("{{{} keys}}", t.len()),
    }
}

/// Emit a commented template for `ndx recall identity edit` to open on a
/// missing file.
pub fn template() -> &'static str {
    r#"# ndx recall identity file (TOML)
# Global file:  ~/.ndx/identity.toml
# Per-project:  {project}/.ndx/identity.toml (merges on top of global)

# name = "Your Name"
# role = "Software engineer"

# notes = """
# Free-form prose. Anything that should always be part of L0.
# """

# [traits]
# style = "direct, terse"

# [people.alice]
# relation = "colleague"
# context = "reviews auth changes"

# [projects.myproject]
# path = "/absolute/path/to/project"
# focus = "what this project is about"
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_tables_recursively() {
        let base: Value = toml::from_str(
            r#"
name = "Base"
[traits]
style = "terse"
"#,
        )
        .unwrap();
        let over: Value = toml::from_str(
            r#"
role = "engineer"
[traits]
prefers = "functional"
"#,
        )
        .unwrap();
        let merged = merge(base, over);
        let t = merged.as_table().unwrap();
        assert_eq!(t.get("name").unwrap().as_str().unwrap(), "Base");
        assert_eq!(t.get("role").unwrap().as_str().unwrap(), "engineer");
        let traits = t.get("traits").unwrap().as_table().unwrap();
        assert_eq!(traits.get("style").unwrap().as_str().unwrap(), "terse");
        assert_eq!(
            traits.get("prefers").unwrap().as_str().unwrap(),
            "functional"
        );
    }

    #[test]
    fn scalar_override_wins() {
        let base: Value = toml::from_str(r#"name = "Old""#).unwrap();
        let over: Value = toml::from_str(r#"name = "New""#).unwrap();
        let merged = merge(base, over);
        assert_eq!(
            merged.as_table().unwrap().get("name").unwrap().as_str().unwrap(),
            "New"
        );
    }

    #[test]
    fn arrays_replace_wholesale() {
        let base: Value = toml::from_str(r#"tags = ["a", "b"]"#).unwrap();
        let over: Value = toml::from_str(r#"tags = ["c"]"#).unwrap();
        let merged = merge(base, over);
        let tags = merged
            .as_table()
            .unwrap()
            .get("tags")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].as_str().unwrap(), "c");
    }

    #[test]
    fn render_l0_none() {
        let out = render_l0(None, None);
        assert!(out.contains("## L0 — IDENTITY"));
        assert!(out.contains("no identity configured"));
    }

    #[test]
    fn render_l0_basic() {
        let v: Value = toml::from_str(
            r#"
name = "Sergiy"
role = "engineer"
notes = "likes Rust"
[traits]
style = "terse"
[projects.ndx]
path = "/tmp/ndx"
focus = "Rust CLI"
[projects.other]
path = "/tmp/other"
"#,
        )
        .unwrap();
        let out = render_l0(Some(&v), Some("ndx"));
        assert!(out.contains("Name: Sergiy"));
        assert!(out.contains("Role: engineer"));
        assert!(out.contains("style: terse"));
        assert!(out.contains("Project: ndx"));
        assert!(out.contains("focus: Rust CLI"));
        assert!(out.contains("Other projects: other"));
        assert!(out.contains("likes Rust"));
    }
}
