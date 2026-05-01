mod client;
mod daemon;
mod hook;
mod index;
mod install;
mod memory;
mod recall;
mod scanner;
mod server;
mod tokens;
mod trigram;
mod watcher;

use anyhow::{Context, Result};
use memory::MemoryIndex;
use recall::{ExitCode, Palace, RecallError};
use std::path::PathBuf;

fn print_usage() {
    eprintln!("ndx {} — Fast File Index & Memory Search CLI", env!("CARGO_PKG_VERSION"));
    eprintln!();
    eprintln!("File index commands (operate on project in current directory):");
    eprintln!("  ndx search <pattern>     Search file contents (trigram-accelerated)");
    eprintln!("    --file-pattern GLOB      Filter files by glob");
    eprintln!("    --max-results N          Limit results (default: 100)");
    eprintln!("    -B N                     Lines of context before match");
    eprintln!("    -A N                     Lines of context after match");
    eprintln!("    --output MODE            content (default), files, count");
    eprintln!("    --offset N               Skip first N results");
    eprintln!("  ndx list                 List indexed files");
    eprintln!("    --path DIR               Filter by directory prefix");
    eprintln!("    --pattern GLOB           Filter by glob pattern");
    eprintln!("    --sort name|modified     Sort order (default: name)");
    eprintln!("  ndx find <pattern>       Find files matching glob pattern");
    eprintln!("    --sort name|modified     Sort order (default: name)");
    eprintln!("  ndx status               Show index + memory statistics");
    eprintln!();
    eprintln!("Recall commands (per-project memory palace, direct access):");
    eprintln!("  ndx recall init                 Create .ndx/recall.redb");
    eprintln!("  ndx recall status [--json]      Palace statistics");
    eprintln!("  ndx recall room <add|list|show|rm|rename>  Room management");
    eprintln!("  ndx recall identity <show|edit> Identity (L0) file (TOML)");
    eprintln!();
    eprintln!("Memory commands (direct access, no daemon needed):");
    eprintln!("  ndx memory search <query>    Search session transcripts");
    eprintln!("  ndx memory events <query>    Search event log");
    eprintln!("  ndx memory list              List recent sessions");
    eprintln!("    --project DIR                Filter by project");
    eprintln!("  ndx memory stats             Show memory statistics");
    eprintln!("  ndx memory session <id>      Get session details");
    eprintln!("  ndx memory context           Recent project context");
    eprintln!("    --project DIR                Filter by project");
    eprintln!("  ndx memory subagents <query> Search subagent transcripts");
    eprintln!("    --parent ID                  Filter by parent session");
    eprintln!("  ndx memory tree <id>         Show session + subagent tree");
    eprintln!("  --limit N                    Limit results (all memory commands)");
    eprintln!();
    eprintln!("Cross-reference commands:");
    eprintln!("  ndx xref file <path>          Find sessions that touched a file");
    eprintln!("  ndx xref session <id>         List files touched by a session");
    eprintln!("  ndx xref drawer <file>        Find drawers that reference a file");
    eprintln!("  ndx xref drawer-session <id>  Find drawers derived from a session");
    eprintln!("  ndx xref git <commit>         Find drawers referencing files changed in a commit");
    eprintln!("  --limit N                     Limit results");
    eprintln!();
    eprintln!("Daemon commands:");
    eprintln!("  ndx stop                 Stop the background daemon");
    eprintln!("  ndx ping                 Check if daemon is running");
    eprintln!();
    eprintln!("Other commands:");
    eprintln!("  ndx issue <add|list|show|close|reopen|update|rm|milestones>");
    eprintln!("                           Per-project issue tracker (drawers in `_issues_`)");
    eprintln!("  ndx scan                 Scan memory (sessions, events, agents)");
    eprintln!("  ndx hook                 PreToolUse hook handler (stdin/stdout)");
    eprintln!("  ndx filter <key>         Output noise filter (stdin/stdout)");
    eprintln!("  ndx install              Download manifests, register hook + skill");
    eprintln!("  ndx init [path] [--clean-up]");
    eprintln!("                           Wire ndx into a project (CLAUDE.md, .gitignore).");
    eprintln!("                           --clean-up removes pre-existing project-local skill copies.");
    eprintln!("  ndx help                 Show this help message");
}

// ── Argument parsing helpers ──

fn get_flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn get_flag_usize(args: &[String], flag: &str) -> Option<usize> {
    get_flag(args, flag).and_then(|v| v.parse().ok())
}

fn get_positional<'a>(args: &'a [String], skip_flags: &[&str]) -> Option<&'a str> {
    for (i, arg) in args.iter().enumerate() {
        if arg.starts_with('-') {
            continue;
        }
        if i > 0 && skip_flags.contains(&args[i - 1].as_str()) {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn project_root() -> Result<PathBuf> {
    let root = std::env::current_dir().context("failed to get current directory")?;
    Ok(root.canonicalize().unwrap_or(root))
}

fn open_memory() -> Option<MemoryIndex> {
    MemoryIndex::open().ok()
}

// ── Index commands (via daemon) ──

fn cmd_search(args: &[String]) -> Result<()> {
    let pattern = get_positional(
        args,
        &["--file-pattern", "--max-results", "--output", "--offset", "-B", "-A"],
    )
    .context("usage: ndx search <pattern>")?;

    let mut params = serde_json::json!({"pattern": pattern});
    if let Some(fp) = get_flag(args, "--file-pattern") {
        params["file_pattern"] = serde_json::json!(fp);
    }
    if let Some(n) = get_flag_usize(args, "--max-results") {
        params["max_results"] = serde_json::json!(n);
    }
    if let Some(n) = get_flag_usize(args, "-B") {
        params["before_context"] = serde_json::json!(n);
    }
    if let Some(n) = get_flag_usize(args, "-A") {
        params["after_context"] = serde_json::json!(n);
    }
    if let Some(m) = get_flag(args, "--output") {
        params["output_mode"] = serde_json::json!(m);
    }
    if let Some(n) = get_flag_usize(args, "--offset") {
        params["offset"] = serde_json::json!(n);
    }

    let root = project_root()?;
    let result = client::query(&root, "search_content", params)?;
    if !result.is_empty() {
        println!("{}", result);
    }
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<()> {
    let mut params = serde_json::json!({});
    if let Some(p) = get_flag(args, "--path") {
        params["path"] = serde_json::json!(p);
    }
    if let Some(p) = get_flag(args, "--pattern") {
        params["pattern"] = serde_json::json!(p);
    }
    if let Some(s) = get_flag(args, "--sort") {
        params["sort"] = serde_json::json!(s);
    }
    if has_flag(args, "--tokens") {
        params["tokens"] = serde_json::json!(true);
    }
    if has_flag(args, "--json") {
        params["json"] = serde_json::json!(true);
    }

    let root = project_root()?;
    let result = client::query(&root, "list_files", params)?;
    if !result.is_empty() {
        println!("{}", result);
    }
    Ok(())
}

fn cmd_find(args: &[String]) -> Result<()> {
    let pattern =
        get_positional(args, &["--sort"]).context("usage: ndx find <glob-pattern>")?;

    let mut params = serde_json::json!({"pattern": pattern});
    if let Some(s) = get_flag(args, "--sort") {
        params["sort"] = serde_json::json!(s);
    }
    if has_flag(args, "--tokens") {
        params["tokens"] = serde_json::json!(true);
    }
    if has_flag(args, "--json") {
        params["json"] = serde_json::json!(true);
    }

    let root = project_root()?;
    let result = client::query(&root, "search_files", params)?;
    if !result.is_empty() {
        println!("{}", result);
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let root = project_root()?;
    let index_stats = client::query(&root, "index_status", serde_json::json!({}))?;
    println!("{}", index_stats);

    // Add memory stats (direct access)
    if let Some(mem) = open_memory() {
        match mem.session_stats() {
            Ok(stats) => {
                println!(
                    "\nMemory:\n  Sessions: {}\n  Events: {}\n  Agents: {}\n  Total turns: {}\n  Total tool calls: {}",
                    stats.session_count, stats.event_count, stats.agent_count,
                    stats.total_turns, stats.total_tool_calls
                );
                if let Some(ref oldest) = stats.oldest_session {
                    println!("  Oldest session: {}", oldest);
                }
                if let Some(ref newest) = stats.newest_session {
                    println!("  Newest session: {}", newest);
                }
            }
            Err(e) => println!("\nMemory: error: {}", e),
        }
    }
    Ok(())
}

// ── Memory commands (direct access) ──

fn cmd_memory(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str());
    let sub_args = if args.len() > 1 { &args[1..] } else { &[] };

    let mem = MemoryIndex::open().context("failed to open memory database")?;
    let limit = get_flag_usize(args, "--limit");

    let result = match sub {
        Some("search") => {
            let query = get_positional(sub_args, &["--limit"])
                .context("usage: ndx memory search <query>")?;
            server::memory_search(&mem, query, limit.unwrap_or(20))
        }
        Some("events") => {
            let query = get_positional(sub_args, &["--limit"])
                .context("usage: ndx memory events <query>")?;
            server::memory_events_search(&mem, query, limit.unwrap_or(50))
        }
        Some("list") => {
            let project = get_flag(args, "--project");
            server::memory_list(&mem, project, limit.unwrap_or(20))
        }
        Some("stats") => server::memory_stats(&mem),
        Some("session") => {
            let id = get_positional(sub_args, &["--limit"])
                .context("usage: ndx memory session <id>")?;
            server::memory_session_detail(&mem, id)
        }
        Some("context") => {
            let project = get_flag(args, "--project");
            server::memory_project_context(&mem, project)
        }
        Some("subagents") => {
            let query = get_positional(sub_args, &["--limit", "--parent"])
                .context("usage: ndx memory subagents <query>")?;
            let parent = get_flag(args, "--parent");
            server::memory_subagent_search(&mem, query, parent, limit.unwrap_or(20))
        }
        Some("tree") => {
            let id = get_positional(sub_args, &[])
                .context("usage: ndx memory tree <session_id>")?;
            server::memory_session_tree(&mem, id)
        }
        _ => {
            anyhow::bail!("unknown memory subcommand. Run 'ndx help' for usage.");
        }
    };

    match result {
        Ok(output) => println!("{}", output),
        Err(e) => anyhow::bail!("{}", e),
    }
    Ok(())
}

// ── Cross-reference commands ──

fn cmd_xref(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str());
    let sub_args: &[String] = if args.len() > 1 { &args[1..] } else { &[] };
    let limit = get_flag_usize(args, "--limit");

    // Palace-backed xrefs are Phase 4 additions.
    match sub {
        Some("drawer") => return cmd_xref_drawer(sub_args),
        Some("drawer-session") => return cmd_xref_drawer_session(sub_args),
        Some("git") => return cmd_xref_git(sub_args),
        _ => {}
    }

    // Legacy session/file xrefs over the global memory index.
    let mem = MemoryIndex::open().context("failed to open memory database")?;
    let result = match sub {
        Some("file") => {
            let path = get_positional(sub_args, &["--limit"])
                .context("usage: ndx xref file <path>")?;
            server::file_sessions(&mem, path, limit.unwrap_or(10))
        }
        Some("session") => {
            let id = get_positional(sub_args, &["--limit"])
                .context("usage: ndx xref session <session_id>")?;
            let root = project_root()?;
            server::session_files(&mem, &root, id)
        }
        _ => {
            anyhow::bail!("unknown xref subcommand. Run 'ndx help' for usage.");
        }
    };

    match result {
        Ok(output) => println!("{}", output),
        Err(e) => anyhow::bail!("{}", e),
    }
    Ok(())
}

fn cmd_xref_drawer(args: &[String]) -> Result<()> {
    let path = get_positional(args, &["--limit"])
        .ok_or_else(|| RecallError::usage("usage: ndx xref drawer <file>"))?;
    let palace = Palace::open_from_cwd()?;
    let limit = get_flag_usize(args, "--limit").unwrap_or(20);
    // R-1023: `drawers_for_file` now resolves its input against
    // canonical_root internally — pass the raw path straight through.
    let mut hits = palace.drawers_for_file(path)?;
    hits.truncate(limit);
    render_drawer_hits(&hits, args.iter().any(|a| a == "--json"))
}

fn cmd_xref_drawer_session(args: &[String]) -> Result<()> {
    let id = get_positional(args, &["--limit"])
        .ok_or_else(|| RecallError::usage("usage: ndx xref drawer-session <session-id>"))?;
    let palace = Palace::open_from_cwd()?;
    let limit = get_flag_usize(args, "--limit").unwrap_or(50);
    let mut hits = palace.drawers_for_session(id)?;
    hits.truncate(limit);
    render_drawer_hits(&hits, args.iter().any(|a| a == "--json"))
}

fn cmd_xref_git(args: &[String]) -> Result<()> {
    let commit = get_positional(args, &["--limit"])
        .ok_or_else(|| RecallError::usage("usage: ndx xref git <commit>"))?;
    let palace = Palace::open_from_cwd()?;
    let limit = get_flag_usize(args, "--limit").unwrap_or(50);
    let mut hits = palace.drawers_for_commit(commit)?;
    hits.truncate(limit);
    render_drawer_hits(&hits, args.iter().any(|a| a == "--json"))
}

fn render_drawer_hits(hits: &[recall::Drawer], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("(no drawers)");
        return Ok(());
    }
    for d in hits {
        let snippet: String = d
            .text
            .chars()
            .take(200)
            .collect::<String>()
            .replace('\n', " ");
        let src = match (&d.source_file, &d.source_session_id) {
            (Some(f), _) => format!("  src: {}", f),
            (None, Some(s)) => format!("  session: {}", recall::safe_prefix(s, 8)),
            _ => String::new(),
        };
        println!("[{:>5}] [{}] i={}{}", d.id, d.room, d.importance, src);
        println!("        {}", snippet);
    }
    Ok(())
}

// ── Hook/filter commands ──

/// Top-level issue tracker dispatcher. Issues live in the recall
/// palace's reserved `_issues_` room (see `recall::issue`); this
/// function wires the user-facing CLI to those primitives.
fn cmd_issue(args: &[String]) -> Result<()> {
    use recall::issue;

    let sub = args.first().map(|s| s.as_str());
    let sub_args: &[String] = if args.len() > 1 { &args[1..] } else { &[] };

    match sub {
        Some("add") => {
            let title = get_positional(
                sub_args,
                &[
                    "--body",
                    "--milestone",
                    "--importance",
                    "--source-file",
                    "--link-drawer",
                ],
            )
            .ok_or_else(|| {
                RecallError::usage(
                    "usage: ndx issue add \"title\" [--body B] [--milestone M] [--importance N] [--source-file F] [--link-drawer N]",
                )
            })?;
            let body = get_flag(sub_args, "--body");
            let milestone = get_flag(sub_args, "--milestone");
            let importance = get_flag_usize(sub_args, "--importance")
                .map(|n| n as u8)
                .unwrap_or(recall::DEFAULT_IMPORTANCE);
            let source_file = get_flag(sub_args, "--source-file");
            let mut link_drawers = Vec::new();
            for w in sub_args.windows(2) {
                if w[0] == "--link-drawer" {
                    if let Ok(n) = w[1].parse::<u64>() {
                        link_drawers.push(n);
                    }
                }
            }

            let palace = Palace::open_from_cwd()?;
            let outcome = issue::add(
                &palace,
                issue::AddOptions {
                    title,
                    body,
                    milestone,
                    importance,
                    source_file,
                    link_drawers: &link_drawers,
                },
            )?;
            if has_flag(sub_args, "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "id": outcome.id,
                        "status": "open",
                        "milestone": milestone,
                    }))?
                );
            } else {
                eprintln!("issue {} filed", outcome.id);
            }
            Ok(())
        }

        Some("list") => {
            let status = match get_flag(sub_args, "--status") {
                None => issue::StatusFilter::Open,
                Some(s) => issue::StatusFilter::parse(s).ok_or_else(|| {
                    RecallError::usage("--status must be one of: open|closed|all")
                })?,
            };
            let milestone = get_flag(sub_args, "--milestone");
            let palace = Palace::open_from_cwd()?;
            let issues = issue::list(&palace, status, milestone)?;

            if has_flag(sub_args, "--json") {
                println!("{}", serde_json::to_string_pretty(&issues)?);
                return Ok(());
            }
            if issues.is_empty() {
                println!("(no issues)");
                return Ok(());
            }
            for d in &issues {
                let st = issue::drawer_status(d);
                let ms = d
                    .metadata
                    .get(issue::META_MILESTONE)
                    .map(|s| s.as_str())
                    .unwrap_or("-");
                let title = d.text.lines().next().unwrap_or("(untitled)");
                println!(
                    "#{:<5} [{}] imp={} m={} — {}",
                    d.id, st, d.importance, ms, title
                );
            }
            Ok(())
        }

        Some("show") => {
            let id = get_positional(sub_args, &[])
                .ok_or_else(|| RecallError::usage("usage: ndx issue show <id>"))?
                .parse::<u64>()
                .map_err(|_| RecallError::usage("issue id must be a positive integer"))?;
            let palace = Palace::open_from_cwd()?;
            let drawers = palace.list_drawers(Some(issue::ISSUES_ROOM), usize::MAX, 0)?;
            let d = drawers
                .into_iter()
                .find(|d| d.id == id)
                .ok_or_else(|| RecallError::constraint(format!("issue {} not found", id)))?;
            if has_flag(sub_args, "--json") {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                println!("#{}  status={}  importance={}", d.id, issue::drawer_status(&d), d.importance);
                if let Some(ms) = d.metadata.get(issue::META_MILESTONE) {
                    println!("milestone: {}", ms);
                }
                if let Some(ts) = d.metadata.get(issue::META_CLOSED_AT) {
                    println!("closed_at: {}", ts);
                }
                println!();
                println!("{}", d.text);
            }
            Ok(())
        }

        Some("close") => {
            let id = get_positional(sub_args, &["--fix", "--commit", "--link-drawer"])
                .ok_or_else(|| {
                    RecallError::usage(
                        "usage: ndx issue close <id> [--fix S] [--commit C] [--link-drawer N]",
                    )
                })?
                .parse::<u64>()
                .map_err(|_| RecallError::usage("issue id must be a positive integer"))?;
            let fix = get_flag(sub_args, "--fix");
            let commit = get_flag(sub_args, "--commit");
            let link_drawer = get_flag_usize(sub_args, "--link-drawer").map(|n| n as u64);
            let palace = Palace::open_from_cwd()?;
            issue::close(
                &palace,
                id,
                issue::CloseOptions {
                    fix,
                    commit,
                    link_drawer,
                },
            )?;
            eprintln!("issue {} closed", id);
            Ok(())
        }

        Some("reopen") => {
            let id = get_positional(sub_args, &[])
                .ok_or_else(|| RecallError::usage("usage: ndx issue reopen <id>"))?
                .parse::<u64>()
                .map_err(|_| RecallError::usage("issue id must be a positive integer"))?;
            let palace = Palace::open_from_cwd()?;
            issue::reopen(&palace, id)?;
            eprintln!("issue {} reopened", id);
            Ok(())
        }

        Some("update") => {
            let id = get_positional(sub_args, &["--milestone", "--importance"])
                .ok_or_else(|| {
                    RecallError::usage(
                        "usage: ndx issue update <id> [--milestone M] [--importance N]",
                    )
                })?
                .parse::<u64>()
                .map_err(|_| RecallError::usage("issue id must be a positive integer"))?;
            let palace = Palace::open_from_cwd()?;

            // --milestone "" or absence means "leave as is" unless the
            // flag itself is present; the explicit "" clears it.
            if let Some(ms) = get_flag(sub_args, "--milestone") {
                issue::set_milestone(&palace, id, Some(ms))?;
            }
            if let Some(imp) = get_flag_usize(sub_args, "--importance") {
                palace.update_drawer(id, None, Some(imp as u8), None)?;
            }
            eprintln!("issue {} updated", id);
            Ok(())
        }

        Some("rm") => {
            let id = get_positional(sub_args, &[])
                .ok_or_else(|| RecallError::usage("usage: ndx issue rm <id>"))?
                .parse::<u64>()
                .map_err(|_| RecallError::usage("issue id must be a positive integer"))?;
            let palace = Palace::open_from_cwd()?;
            let removed = palace.delete_drawer(id)?;
            if removed {
                eprintln!("issue {} removed", id);
            } else {
                eprintln!("issue {} not found", id);
            }
            Ok(())
        }

        Some("milestones") => {
            let palace = Palace::open_from_cwd()?;
            let summary = issue::milestone_summary(&palace)?;
            if has_flag(sub_args, "--json") {
                println!("{}", serde_json::to_string_pretty(&summary)?);
                return Ok(());
            }
            if summary.is_empty() {
                println!("(no issues)");
                return Ok(());
            }
            for m in &summary {
                println!(
                    "{:<20} {} open / {} closed",
                    m.milestone, m.open, m.closed
                );
            }
            Ok(())
        }

        Some(other) => Err(RecallError::usage(format!(
            "unknown `ndx issue` subcommand: `{}`. expected: add | list | show | close | reopen | update | rm | milestones",
            other
        ))
        .into()),

        None => {
            eprintln!("usage: ndx issue <add|list|show|close|reopen|update|rm|milestones> [args]");
            Ok(())
        }
    }
}

fn cmd_hook() -> Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;

    // Parse once for reuse by both the manifest-driven handler and the
    // Phase 5 wake-up injection path.
    let hook_input: Option<hook::HookInput> = serde_json::from_str(&input).ok();

    // Dispatch on hook event name. PreCompact, SessionStart and
    // SessionEnd each have their own output schema (or no output at
    // all) and must short-circuit before the PreToolUse Bash flow.
    if let Some(hi) = hook_input.as_ref() {
        match hi.hook_event_name.as_deref() {
            Some("PreCompact") => {
                match build_precompact_output(hi) {
                    Ok(Some(out)) => {
                        println!("{}", serde_json::to_string(&out)?);
                    }
                    Ok(None) => {} // silent soft-skip
                    Err(e) => {
                        eprintln!("[ndx hook] PreCompact injection skipped: {}", e);
                    }
                }
                return Ok(());
            }
            Some("SessionStart") => {
                match build_session_start_output(hi) {
                    Ok(Some(out)) => {
                        println!("{}", serde_json::to_string(&out)?);
                    }
                    Ok(None) => {} // silent soft-skip (no palace, etc.)
                    Err(e) => {
                        eprintln!("[ndx hook] SessionStart skipped: {}", e);
                    }
                }
                return Ok(());
            }
            Some("SessionEnd") => {
                if let Err(e) = handle_session_end(hi) {
                    eprintln!("[ndx hook] SessionEnd skipped: {}", e);
                }
                return Ok(());
            }
            _ => {}
        }

        // PreToolUse on Read: emit the repeat-read nudge (if any) and
        // record the event for the next iteration. Short-circuit before
        // the Bash flow — Read events have nothing to do with manifest
        // lookups or wake-up injection.
        if hi.hook_event_name.as_deref() == Some("PreToolUse")
            && hi.tool_name.as_deref() == Some("Read")
        {
            handle_read_hook(hi);
            return Ok(());
        }
    }

    // Phase A/B: manifest-driven hook response.
    let mut response = match hook::handle_hook(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[ndx hook] manifest handler error: {}", e);
            None
        }
    };

    // Phase 5: wake-up injection (first Bash command per Claude session).
    // Soft-fail: any error here must not break the existing hook output.
    if let Some(ref hi) = hook_input {
        if let Err(e) = try_inject_wake_up(hi, &mut response) {
            eprintln!("[ndx hook] wake-up injection skipped: {}", e);
        }
    }

    if let Some(resp) = response {
        println!("{}", serde_json::to_string(&resp)?);
    }

    // Phase C: log event to memory (best-effort)
    if let Some(hook_input) = hook_input {
        if let Some(command) = hook_input
            .tool_input
            .as_ref()
            .and_then(|ti| ti.command.as_deref())
        {
            let parsed = hook::parser::parse_command(command);
            let manifest_key = parsed.as_ref().map(|p| p.key.clone());

            if let Ok(mem) = MemoryIndex::open() {
                let entry = memory::EventEntry {
                    event_ts: chrono::Utc::now().to_rfc3339(),
                    session_id: hook_input.session_id.unwrap_or_default(),
                    project_dir: hook_input.cwd.unwrap_or_default(),
                    tool: "Bash".to_string(),
                    command: recall::safe_prefix(command, 500).to_string(),
                    manifest_key,
                    ingested_at: chrono::Utc::now().to_rfc3339(),
                    meta: None,
                };
                let _ = mem.insert_event(&entry);
            }
        }
    }

    Ok(())
}

/// Prepend L0+L1 wake-up text to the hook's `additional_context` on the
/// first Bash hook invocation per Claude session. Scopes to the palace
/// rooted at the hook payload's `cwd`. Missing session id, missing cwd,
/// missing palace, and missing model all silently skip (soft-fail).
/// Spec: R-800..R-805.
fn try_inject_wake_up(
    hi: &hook::HookInput,
    response: &mut Option<hook::HookOutput>,
) -> Result<()> {
    // Require a Bash invocation — injection piggy-backs on Bash PreToolUse.
    if hi.tool_name.as_deref() != Some("Bash") {
        return Ok(());
    }
    let session_id = match hi.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(()),
    };
    let cwd = match hi.cwd.as_deref() {
        Some(c) if !c.is_empty() => std::path::PathBuf::from(c),
        _ => return Ok(()),
    };

    // Walk up for a palace, silently skip if none exists.
    let root = match find_palace_root(&cwd) {
        Some(r) => r,
        None => return Ok(()),
    };
    let palace = Palace::open_at(root)?;

    if palace.wake_injection_seen(session_id)? {
        return Ok(());
    }

    let wake_text = recall::search::wake_up(&palace)?;
    let block = format!(
        "# ndx-recall wake-up (session {})\n{}\n# /wake-up\n",
        recall::safe_prefix(session_id, 8),
        wake_text.trim_end()
    );

    // Either prepend to existing response's additional_context, or create
    // a minimal response that carries just the wake-up block.
    match response {
        Some(resp) => {
            let existing = resp
                .hook_specific_output
                .additional_context
                .take()
                .unwrap_or_default();
            resp.hook_specific_output.additional_context = Some(if existing.is_empty() {
                block
            } else {
                format!("{}\n{}", block, existing)
            });
        }
        None => {
            *response = Some(hook::HookOutput {
                hook_specific_output: hook::HookSpecificOutput {
                    hook_event_name: "PreToolUse".to_string(),
                    permission_decision: "allow".to_string(),
                    additional_context: Some(block),
                    updated_input: None,
                },
            });
        }
    }

    palace.mark_wake_injected(session_id)?;
    Ok(())
}

/// PreCompact wake-up injection.
///
/// Claude Code calls `ndx hook` with `hook_event_name == "PreCompact"`
/// before compacting a session (either manual `/compact` or automatic
/// at the context limit). We re-inject the L0+L1 palace wake-up text
/// via `hookSpecificOutput.additionalContext` so the palace context
/// survives compaction.
///
/// Unlike the PreToolUse/Bash path, PreCompact:
///   - ignores the `WAKE_INJECTED` per-session gate (different channel),
///   - returns a narrower `hookSpecificOutput` (no permissionDecision).
///
/// All failure modes (missing cwd, missing palace, I/O error) return
/// `Ok(None)` — soft-fail, no output, exit 0 — matching the PreToolUse
/// handler's behavior. `Err(...)` is reserved for programming errors
/// that the caller surfaces on stderr.
fn build_precompact_output(
    hi: &hook::HookInput,
) -> Result<Option<hook::PreCompactOutput>> {
    let cwd = match hi.cwd.as_deref() {
        Some(c) if !c.is_empty() => std::path::PathBuf::from(c),
        _ => return Ok(None),
    };
    let root = match find_palace_root(&cwd) {
        Some(r) => r,
        None => return Ok(None),
    };
    let palace = match Palace::open_at(root) {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    let wake_text = match recall::search::wake_up(&palace) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };

    let session_label = hi
        .session_id
        .as_deref()
        .map(|s| recall::safe_prefix(s, 8).to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let trigger = hi.trigger.as_deref().unwrap_or("unknown");
    let block = format!(
        "# ndx-recall wake-up (pre-compact session {} trigger={})\n{}\n# /wake-up\n",
        session_label,
        trigger,
        wake_text.trim_end(),
    );

    Ok(Some(hook::PreCompactOutput {
        hook_specific_output: hook::PreCompactSpecificOutput {
            hook_event_name: "PreCompact".to_string(),
            additional_context: Some(block),
        },
    }))
}

/// Threshold for emitting the `/ndx-chore` nudge from the SessionStart
/// hook. Sum of pending classify + score + dedupe + contradict drawers.
const SESSION_START_NUDGE_THRESHOLD: usize = 20;

/// SessionStart hook handler.
///
/// Behavior:
///   1. Walk up from `cwd` for a palace; soft-fail (Ok(None)) if missing.
///   2. Auto-mine since `last_mined_at` (no embed). Best-effort: log on
///      failure, continue to the nudge.
///   3. Count pending hygiene drawers across the four `/ndx-chore`
///      phases. If sum ≥ threshold, emit `additionalContext` nudging
///      the user toward `/ndx-chore`. Below threshold: Ok(None).
///
/// Soft-fail philosophy mirrors PreCompact: any error returns Ok(None)
/// (no output, exit 0) rather than disrupting the launch. `Err` is
/// reserved for programming bugs the caller surfaces on stderr.
fn build_session_start_output(
    hi: &hook::HookInput,
) -> Result<Option<hook::SessionStartOutput>> {
    let cwd = match hi.cwd.as_deref() {
        Some(c) if !c.is_empty() => std::path::PathBuf::from(c),
        _ => return Ok(None),
    };
    let root = match find_palace_root(&cwd) {
        Some(r) => r,
        None => return Ok(None),
    };
    let palace = match Palace::open_at(root) {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    // Best-effort auto-mine. Any error here must not block the nudge.
    if let Err(e) = recall::mine::mine_from_memory_with_opts(
        &palace,
        recall::mine::MineFromMemoryOpts::default(),
    ) {
        eprintln!("[ndx hook] SessionStart auto-mine skipped: {}", e);
    }

    session_start_nudge_for(&palace)
}

/// Pure subroutine of [`build_session_start_output`]: given a palace,
/// produce the SessionStart nudge if the hygiene backlog is at or above
/// [`SESSION_START_NUDGE_THRESHOLD`]. Skips the auto-mine side effect so
/// it can be exercised in unit tests without touching the global memory
/// database.
fn session_start_nudge_for(
    palace: &Palace,
) -> Result<Option<hook::SessionStartOutput>> {
    // Count pending across the four /ndx-chore phases. Use a generous
    // limit so we get a meaningful count rather than a clipped value
    // when the backlog is very large.
    let (n_classify, n_score, n_dedupe, n_contradict) =
        pending_hygiene_counts(palace).unwrap_or((0, 0, 0, 0));
    let total = n_classify + n_score + n_dedupe + n_contradict;

    if total < SESSION_START_NUDGE_THRESHOLD {
        return Ok(None);
    }

    let block = format!(
        "# ndx-recall — palace hygiene pending\n\
         {} drawers need classification, {} need importance scoring,\n\
         {} dedupe candidates, {} contradictions.\n\
         Run `/ndx-chore` to work through them.\n",
        n_classify, n_score, n_dedupe, n_contradict,
    );

    Ok(Some(hook::SessionStartOutput {
        hook_specific_output: hook::SessionStartSpecificOutput {
            hook_event_name: "SessionStart".to_string(),
            additional_context: Some(block),
        },
    }))
}

/// Sum of pending counts across classify/score/dedupe/contradict.
/// Each is capped at a generous limit; we only need an order-of-magnitude
/// signal, not an exact count.
fn pending_hygiene_counts(palace: &Palace) -> Result<(usize, usize, usize, usize)> {
    use recall::PendingOp;
    const CAP: usize = 1000;
    let n_classify = palace.list_pending(PendingOp::Classify, CAP)?.len();
    let n_score = palace.list_pending(PendingOp::Score, CAP)?.len();
    let n_dedupe = palace.list_pending(PendingOp::Dedupe, CAP)?.len();
    let n_contradict = palace.list_pending(PendingOp::Contradict, CAP)?.len();
    Ok((n_classify, n_score, n_dedupe, n_contradict))
}

/// SessionEnd hook handler. Observational — no JSON output, exit 0.
///
/// Repeated-read detection. Fires before every Read tool invocation.
///
/// Captures the file's current `mtime` and counts past Read events for
/// the same `(session_id, file_path, mtime)` tuple. Two prior reads of
/// identical content (i.e. no Edit/Write bumped mtime in between)
/// means the upcoming read would be the third — emit an
/// `additionalContext` nudge so Claude works from existing context
/// instead of paying the read cost again. Then log the event so the
/// next call can see this read.
///
/// Soft-fails on every error: missing file, unreadable metadata,
/// memory DB unavailable. Read-only side effects from Claude's
/// perspective; we never block or rewrite the read.
const REPEAT_READ_THRESHOLD: usize = 2;

fn handle_read_hook(hi: &hook::HookInput) {
    let path = match hi
        .tool_input
        .as_ref()
        .and_then(|ti| ti.file_path.as_deref())
    {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => return,
    };
    let session_id = hi.session_id.clone().unwrap_or_default();
    if session_id.is_empty() {
        return;
    }

    let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs().to_string(),
            Err(_) => return,
        },
        Err(_) => return, // file gone / not readable; let Claude's Read fail naturally
    };

    let mem = match MemoryIndex::open() {
        Ok(m) => m,
        Err(_) => return,
    };

    let count = mem
        .count_session_reads(&session_id, &path, &mtime)
        .unwrap_or(0);

    // Emit nudge before logging this read — count reflects past reads
    // only, so threshold==2 means the *next* (current) read is the 3rd.
    if count >= REPEAT_READ_THRESHOLD {
        let msg = format!(
            "ndx: this session has read {} {} times — work from existing context instead of re-reading",
            path,
            count
        );
        let out = hook::HookOutput {
            hook_specific_output: hook::HookSpecificOutput {
                hook_event_name: "PreToolUse".to_string(),
                permission_decision: "allow".to_string(),
                additional_context: Some(msg),
                updated_input: None,
            },
        };
        if let Ok(json) = serde_json::to_string(&out) {
            println!("{}", json);
        }
    }

    // Best-effort log — failures here only mean future hooks may
    // miscount, never break the current Read.
    let entry = memory::EventEntry {
        event_ts: chrono::Utc::now().to_rfc3339(),
        session_id,
        project_dir: hi.cwd.clone().unwrap_or_default(),
        tool: "Read".to_string(),
        command: path,
        manifest_key: None,
        ingested_at: chrono::Utc::now().to_rfc3339(),
        meta: Some(mtime),
    };
    let _ = mem.insert_event(&entry);
}

/// Mines the just-ended session into the palace (raw, no embed). Reuses
/// `MINED_SESSIONS` for idempotency: if the session was already mined
/// (e.g. an earlier SessionStart already covered it), this becomes a
/// no-op for that session.
fn handle_session_end(hi: &hook::HookInput) -> Result<()> {
    let cwd = match hi.cwd.as_deref() {
        Some(c) if !c.is_empty() => std::path::PathBuf::from(c),
        _ => return Ok(()),
    };
    let root = match find_palace_root(&cwd) {
        Some(r) => r,
        None => return Ok(()),
    };
    let palace = match Palace::open_at(root) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let session_id = match hi.session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            // Without a session_id we cannot scope; soft-skip.
            return Ok(());
        }
    };

    let mut allow = std::collections::HashSet::new();
    allow.insert(session_id);

    if let Err(e) = recall::mine::mine_from_memory_with_opts(
        &palace,
        recall::mine::MineFromMemoryOpts {
            since: None,
            force: false,
            embed: false,
            session_ids: Some(allow),
        },
    ) {
        eprintln!("[ndx hook] SessionEnd mine skipped: {}", e);
    }
    Ok(())
}

/// Walk up from a starting directory looking for an existing
/// `.ndx/recall.redb`. Mirrors `Palace::find` but accepts a starting
/// path instead of CWD.
fn find_palace_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut cur = start;
    loop {
        if cur.join(".ndx").join("recall.redb").is_file() {
            return Some(cur.to_path_buf());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

fn cmd_filter(key: &str) -> Result<()> {
    hook::filter::run_filter(key)?;
    Ok(())
}

// ── Maintenance commands ──

fn cmd_scan() -> Result<()> {
    let mem = MemoryIndex::open()?;
    let sessions = memory::session::scan_sessions(&mem)?;
    let agents = memory::agent::scan_agents(&mem)?;
    let events = memory::event::ingest_events(&mem)?;

    eprintln!("Memory scan complete");
    eprintln!(
        "  Sessions: {} indexed, {} unchanged, {} errors",
        sessions.indexed, sessions.unchanged, sessions.errors
    );
    eprintln!(
        "  Agents:   {} indexed, {} unchanged",
        agents.indexed, agents.unchanged
    );
    eprintln!("  Events:   {} new", events.new_events);

    Ok(())
}

fn cmd_init(dir: PathBuf, clean_up: bool) -> Result<()> {
    let dir = dir.canonicalize().context("invalid directory path")?;
    install::install_skill_to_project(&dir)?;
    eprintln!(
        "ndx project hooks installed in {}: CLAUDE.md updated, .gitignore updated",
        dir.display()
    );

    if clean_up {
        let report = install::cleanup_project_skills(&dir)?;
        for path in &report.removed {
            let rel = path.strip_prefix(&dir).unwrap_or(path);
            eprintln!("  Removed: {}", rel.display());
        }
        if report.dir_removed {
            eprintln!("  Removed empty directory: .claude/commands/");
        }
        if !report.needs_git_rm.is_empty() {
            eprintln!();
            eprintln!("Refusing to remove git-tracked files. Run:");
            let rels: Vec<String> = report
                .needs_git_rm
                .iter()
                .map(|p| {
                    p.strip_prefix(&dir)
                        .unwrap_or(p)
                        .display()
                        .to_string()
                })
                .collect();
            eprintln!("  git rm {}", rels.join(" "));
            eprintln!("then commit the removal.");
        }
        if report.removed.is_empty() && report.needs_git_rm.is_empty() {
            eprintln!("  No project-local skill files found — nothing to clean up.");
        }
    }
    Ok(())
}

fn cmd_stop() -> Result<()> {
    let root = project_root()?;
    client::stop(&root)
}

fn cmd_ping() -> Result<()> {
    let root = project_root()?;
    match client::query(&root, "ping", serde_json::json!({})) {
        Ok(resp) => println!("{}", resp),
        Err(e) => anyhow::bail!("daemon not reachable: {}", e),
    }
    Ok(())
}

// ── Daemon entry point (internal, spawned by client) ──

async fn cmd_daemon(root: PathBuf) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ndx=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    let root = root.canonicalize().context("invalid project root path")?;
    tracing::info!("ndx daemon starting for {}", root.display());
    daemon::run(root).await
}

// ── Recall commands (direct palace access) ──

fn cmd_recall(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str());
    let sub_args: &[String] = if args.len() > 1 { &args[1..] } else { &[] };
    match sub {
        Some("init") => cmd_recall_init(sub_args),
        Some("status") => cmd_recall_status(sub_args),
        Some("room") => cmd_recall_room(sub_args),
        Some("identity") => cmd_recall_identity(sub_args),
        Some("mine") => cmd_recall_mine(sub_args),
        Some("drawer") => cmd_recall_drawer(sub_args),
        Some("wake") => cmd_recall_wake(sub_args),
        Some("get") => cmd_recall_get(sub_args),
        Some("search") => cmd_recall_search(sub_args),
        Some("reembed") => cmd_recall_reembed(sub_args),
        Some("rebuild-index") => cmd_recall_rebuild_index(sub_args),
        Some("link-palace") => cmd_recall_link_palace(sub_args),
        Some("unlink-palace") => cmd_recall_unlink_palace(sub_args),
        Some("rehome") => cmd_recall_rehome(sub_args),
        Some(other) => Err(RecallError::usage(format!(
            "unknown recall subcommand `{}`. Run `ndx help` for usage.",
            other
        ))
        .into()),
        None => {
            print_recall_usage();
            Ok(())
        }
    }
}

fn print_recall_usage() {
    eprintln!("ndx recall — per-project structured episodic memory palace");
    eprintln!();
    eprintln!("Palace lifecycle:");
    eprintln!("  ndx recall init [--link <canonical-root>]  Create (or link) .ndx/recall.redb");
    eprintln!("  ndx recall link-palace <canonical-root> [--force]  Replace local palace with symlink");
    eprintln!("  ndx recall unlink-palace [--keep]          Remove symlink (optionally keep a local copy)");
    eprintln!("  ndx recall rehome <new-canonical-root>     Rewrite canonical_root in META");
    eprintln!("  ndx recall status [--json]      Palace statistics");
    eprintln!();
    eprintln!("Rooms:");
    eprintln!("  ndx recall room add <name> [--title T] [--description D]");
    eprintln!("  ndx recall room list [--json]");
    eprintln!("  ndx recall room show <name> [--json]");
    eprintln!("  ndx recall room rm <name>");
    eprintln!("  ndx recall room rename <old> <new>");
    eprintln!();
    eprintln!("Mining:");
    eprintln!("  ndx recall mine --from-memory [--since YYYY-MM-DD]");
    eprintln!("  ndx recall mine --from-chroma <chroma-dir> [--wing NAME]");
    eprintln!("  ndx recall mine --project [--path DIR]");
    eprintln!();
    eprintln!("Drawers:");
    eprintln!("  ndx recall drawer list [--room X] [--limit N] [--offset N] [--pending <op>] [--json]");
    eprintln!("  ndx recall drawer show --id N [--json]");
    eprintln!("  ndx recall drawer add \"text\" [--room X] [--importance N] [--source-file F]");
    eprintln!("  ndx recall drawer update --id N [--room X] [--importance N] [--text \"...\"]");
    eprintln!("  ndx recall drawer rm --id N");
    eprintln!("  ndx recall drawer link --from A --to B --kind <references|contradicts|supersedes|derived_from>");
    eprintln!("  ndx recall drawer unlink --from A --to B [--kind <kind>]");
    eprintln!();
    eprintln!("Retrieval:");
    eprintln!("  ndx recall wake                 Emit L0+L1 wake-up text");
    eprintln!("  ndx recall get --room X [--limit N] [--json]");
    eprintln!("  ndx recall search \"query\" [--room X] [--limit N] [--lexical|--semantic|--hybrid] [--json]");
    eprintln!("  ndx recall reembed [--force]    Backfill embeddings (downloads model if needed)");
    eprintln!("  ndx recall rebuild-index        Re-tokenize all drawers into the BM25 lexical index");
    eprintln!();
    eprintln!("Identity:");
    eprintln!("  ndx recall identity show [--merged]");
    eprintln!("  ndx recall identity edit [--project]");
}

fn cmd_recall_init(args: &[String]) -> Result<()> {
    let link = get_flag(args, "--link");

    let root = recall::current_project_root()?;

    if let Some(target_root) = link {
        // R-1031: `ndx recall init --link <canonical-root>` creates a
        // symlink to the canonical palace. Refuses if the target does
        // not yet exist (R-1044). No canonical_root is stamped locally;
        // the target's META is authoritative.
        let target_root = std::path::PathBuf::from(target_root);
        let target_abs = recall::absolute_path(&target_root);
        let target_db = target_abs.join(".ndx").join("recall.redb");
        if !target_db.exists() {
            return Err(RecallError::constraint(format!(
                "target palace does not exist: {} — run `ndx recall init` there first",
                target_db.display()
            ))
            .into());
        }

        let ndx_dir = root.join(".ndx");
        std::fs::create_dir_all(&ndx_dir).with_context(|| {
            format!("creating {}", ndx_dir.display())
        })?;
        let local_db = ndx_dir.join("recall.redb");
        if local_db.exists()
            || std::fs::symlink_metadata(&local_db).is_ok()
        {
            return Err(RecallError::constraint(format!(
                "{} already exists — remove it or use `ndx recall link-palace`",
                local_db.display()
            ))
            .into());
        }

        // Resolve a chain once (R-1043).
        let final_target = resolve_one_hop_symlink(&target_db)?;

        std::os::unix::fs::symlink(&final_target, &local_db).with_context(|| {
            format!(
                "creating symlink {} -> {}",
                local_db.display(),
                final_target.display()
            )
        })?;
        install::ensure_gitignore_entry(&root)?;
        eprintln!(
            "recall palace linked: {} -> {}",
            local_db.display(),
            final_target.display()
        );
        return Ok(());
    }

    let _palace = Palace::create_at(root.clone())?;
    install::ensure_gitignore_entry(&root)?;
    eprintln!(
        "recall palace initialized at {}/.ndx/recall.redb",
        root.display()
    );
    Ok(())
}

/// Resolve a palace path one hop (R-1043): if `path` is itself a
/// symlink, read its target once so every linked checkout points at
/// the canonical directly rather than forming a chain. Absolutizes the
/// result.
fn resolve_one_hop_symlink(path: &std::path::Path) -> Result<std::path::PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            let link = std::fs::read_link(path)
                .with_context(|| format!("readlink {}", path.display()))?;
            let resolved = if link.is_absolute() {
                link
            } else {
                path.parent().unwrap_or(std::path::Path::new("")).join(link)
            };
            Ok(recall::absolute_path(&resolved))
        }
        _ => Ok(recall::absolute_path(path)),
    }
}

fn cmd_recall_status(args: &[String]) -> Result<()> {
    let palace = Palace::open_from_cwd()?;
    let stats = palace.stats()?;
    let json = args.iter().any(|a| a == "--json");

    // Count active (non-superseded) drawers in the Do-Not-Repeat room
    // for both surfaces.
    let dnr_count = palace
        .list_drawers(Some(recall::search::DNR_ROOM), usize::MAX, 0)?
        .into_iter()
        .filter(|d| !palace.is_superseded(d.id).unwrap_or(false))
        .count();
    // Open-issue count (issues live as drawers in `_issues_` keyed by
    // a `meta["issue.status"]` of "open").
    let open_issues = recall::issue::list(&palace, recall::issue::StatusFilter::Open, None)?
        .len();

    if json {
        // R-1072: JSON surface always includes both fields. Serialize via
        // serde_json::Value so `palace_linked_to` renders as `null` when
        // the palace is not a symlink (the default `serde(default)`
        // behaviour already ensures the key is present).
        let mut v = serde_json::to_value(&stats)?;
        if let Some(obj) = v.as_object_mut() {
            obj.entry("canonical_root")
                .or_insert(serde_json::Value::Null);
            obj.entry("palace_linked_to")
                .or_insert(serde_json::Value::Null);
            obj.insert(
                "do_not_repeat_count".to_string(),
                serde_json::Value::Number(dnr_count.into()),
            );
            obj.insert(
                "open_issues".to_string(),
                serde_json::Value::Number(open_issues.into()),
            );
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("Recall palace: {}", palace.db_path().display());
    println!("  Schema version: {}", stats.schema_version);
    println!("  Drawers: {}", stats.drawer_count);
    println!("  Rooms:   {}", stats.room_count);
    println!("  Links:   {}", stats.link_count);
    println!("  Do-Not-Repeat: {} rules", dnr_count);
    println!("  Open issues:   {}", open_issues);
    if let Some(model) = &stats.embedding_model {
        println!("  Embedding model: {}", model);
    } else {
        println!("  Embedding model: (none — Phase 3)");
    }
    if let Some(ts) = stats.last_mined_at {
        println!("  Last mined: {}", format_unix(ts));
    }
    if let Some(ts) = stats.created_at {
        println!("  Created: {}", format_unix(ts));
    }
    // R-1071: canonical_root always if set; linked target only when
    // the local palace file is a symlink.
    if let Some(root) = &stats.canonical_root {
        println!("  Canonical root: {}", root);
    }
    if let Some(linked) = &stats.palace_linked_to {
        println!("  Linked to: {}", linked);
    }
    Ok(())
}

fn cmd_recall_room(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str());
    let sub_args: &[String] = if args.len() > 1 { &args[1..] } else { &[] };
    match sub {
        Some("add") => {
            let name = get_positional(sub_args, &["--title", "--description"])
                .ok_or_else(|| {
                    RecallError::usage("usage: ndx recall room add <name> [--title T] [--description D]")
                })?;
            let title = get_flag(sub_args, "--title").map(|s| s.to_string());
            let description =
                get_flag(sub_args, "--description").map(|s| s.to_string());
            let palace = Palace::open_from_cwd()?;
            let created = palace.ensure_room(name, title, description)?;
            if created {
                eprintln!("room `{}` created", name);
            } else {
                eprintln!("room `{}` already exists", name);
            }
            Ok(())
        }
        Some("list") => {
            let palace = Palace::open_from_cwd()?;
            let rooms = palace.list_rooms()?;
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&rooms)?);
            } else if rooms.is_empty() {
                println!("(no rooms)");
            } else {
                for r in &rooms {
                    print!("{}", r.name);
                    if let Some(t) = &r.title {
                        print!("  [{}]", t);
                    }
                    println!();
                    if let Some(d) = &r.description {
                        println!("    {}", d);
                    }
                }
            }
            Ok(())
        }
        Some("show") => {
            let name = get_positional(sub_args, &[])
                .ok_or_else(|| RecallError::usage("usage: ndx recall room show <name>"))?;
            let palace = Palace::open_from_cwd()?;
            let room = palace.get_room(name)?.ok_or_else(|| {
                RecallError::constraint(format!("room `{}` not found", name))
            })?;
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&room)?);
            } else {
                println!("name: {}", room.name);
                if let Some(t) = &room.title {
                    println!("title: {}", t);
                }
                if let Some(d) = &room.description {
                    println!("description: {}", d);
                }
                println!("created_at: {}", format_unix(room.created_at));
            }
            Ok(())
        }
        Some("rm") => {
            let name = get_positional(sub_args, &[])
                .ok_or_else(|| RecallError::usage("usage: ndx recall room rm <name>"))?;
            let palace = Palace::open_from_cwd()?;
            palace.delete_room(name)?;
            eprintln!("room `{}` removed", name);
            Ok(())
        }
        Some("rename") => {
            // Positional: old new
            let positional: Vec<&str> = sub_args
                .iter()
                .filter(|s| !s.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if positional.len() < 2 {
                return Err(
                    RecallError::usage("usage: ndx recall room rename <old> <new>").into()
                );
            }
            let palace = Palace::open_from_cwd()?;
            let moved = palace.rename_room(positional[0], positional[1])?;
            eprintln!(
                "room `{}` renamed to `{}` ({} drawers moved)",
                positional[0], positional[1], moved
            );
            Ok(())
        }
        Some(other) => Err(RecallError::usage(format!(
            "unknown `recall room` subcommand `{}`",
            other
        ))
        .into()),
        None => Err(RecallError::usage("usage: ndx recall room <add|list|show|rm|rename>").into()),
    }
}

fn cmd_recall_identity(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str());
    let sub_args: &[String] = if args.len() > 1 { &args[1..] } else { &[] };
    match sub {
        Some("show") => {
            let merged_flag = sub_args.iter().any(|a| a == "--merged");
            let root = Palace::find()?.unwrap_or(recall::current_project_root()?);
            if merged_flag {
                let merged = recall::identity::load_merged(&root)?;
                let project_name = recall::project_name(&root);
                println!(
                    "{}",
                    recall::identity::render_l0(merged.as_ref(), Some(&project_name))
                );
            } else {
                let project_path = recall::identity::project_identity_path(&root);
                let global_path = recall::identity::global_identity_path()?;
                if project_path.exists() {
                    println!("# {}", project_path.display());
                    println!("{}", std::fs::read_to_string(&project_path)?);
                } else if global_path.exists() {
                    println!("# {}", global_path.display());
                    println!("{}", std::fs::read_to_string(&global_path)?);
                } else {
                    println!(
                        "(no identity file; run `ndx recall identity edit` to create {})",
                        global_path.display()
                    );
                }
            }
            Ok(())
        }
        Some("edit") => {
            let per_project = sub_args.iter().any(|a| a == "--project");
            let path = if per_project {
                let root = recall::current_project_root()?;
                let p = recall::identity::project_identity_path(&root);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                p
            } else {
                let p = recall::identity::global_identity_path()?;
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                p
            };
            if !path.exists() {
                std::fs::write(&path, recall::identity::template())?;
                eprintln!("created template at {}", path.display());
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let status = std::process::Command::new(&editor).arg(&path).status();
            match status {
                Ok(s) if s.success() => Ok(()),
                Ok(s) => Err(RecallError::new(
                    ExitCode::Generic,
                    format!("editor `{}` exited with status {}", editor, s),
                )
                .into()),
                Err(e) => Err(RecallError::new(
                    ExitCode::Generic,
                    format!("failed to launch editor `{}`: {}", editor, e),
                )
                .into()),
            }
        }
        Some(other) => Err(RecallError::usage(format!(
            "unknown `recall identity` subcommand `{}`",
            other
        ))
        .into()),
        None => {
            Err(RecallError::usage("usage: ndx recall identity <show|edit>").into())
        }
    }
}

fn cmd_recall_mine(args: &[String]) -> Result<()> {
    let palace = Palace::open_from_cwd()?;
    let from_memory = args.iter().any(|a| a == "--from-memory");
    let from_chroma_idx = args.iter().position(|a| a == "--from-chroma");
    let from_project = args.iter().any(|a| a == "--project");
    let embed = args.iter().any(|a| a == "--embed");
    let force = args.iter().any(|a| a == "--force");

    let mode_count = [from_memory, from_chroma_idx.is_some(), from_project]
        .iter()
        .filter(|b| **b)
        .count();
    if mode_count != 1 {
        return Err(RecallError::usage(
            "usage: ndx recall mine <--from-memory | --from-chroma <dir> | --project [--path DIR]> [--embed] [--force]",
        )
        .into());
    }

    let report = if from_memory {
        let since = get_flag(args, "--since");
        recall::mine::mine_from_memory(&palace, since, force, embed)?
    } else if let Some(idx) = from_chroma_idx {
        let chroma_dir = args
            .get(idx + 1)
            .filter(|s| !s.starts_with("--"))
            .ok_or_else(|| {
                RecallError::usage("usage: ndx recall mine --from-chroma <chroma-dir>")
            })?;
        let wing = get_flag(args, "--wing");
        recall::mine::mine_from_chroma(
            &palace,
            std::path::Path::new(chroma_dir),
            wing,
            embed,
        )?
    } else {
        let path = get_flag(args, "--path").map(std::path::PathBuf::from);
        recall::mine::mine_project(&palace, path.as_deref(), embed)?
    };

    eprintln!(
        "mine: added {}, deduped {}, skipped {}",
        report.added, report.deduped, report.skipped
    );
    if !embed && report.added > 0 {
        eprintln!(
            "  (embeddings skipped — run `ndx recall reembed` or `ndx recall search` to generate)"
        );
    }
    Ok(())
}

fn cmd_recall_drawer(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str());
    let sub_args: &[String] = if args.len() > 1 { &args[1..] } else { &[] };
    match sub {
        Some("list") => {
            let palace = Palace::open_from_cwd()?;
            let room = get_flag(sub_args, "--room");
            let limit = get_flag_usize(sub_args, "--limit").unwrap_or(20);
            let offset = get_flag_usize(sub_args, "--offset").unwrap_or(0);
            let pending = get_flag(sub_args, "--pending");
            let json = sub_args.iter().any(|a| a == "--json");

            // --pending overrides --room: returns the batch for a skill.
            if let Some(pending_name) = pending {
                let op = recall::PendingOp::parse(pending_name).ok_or_else(|| {
                    RecallError::usage(format!(
                        "unknown --pending op `{}`; expected classify|score|dedupe|contradict|summarize",
                        pending_name
                    ))
                })?;
                let drawers = palace.list_pending(op, limit)?;
                if json {
                    let rooms: Vec<String> =
                        palace.list_rooms()?.into_iter().map(|r| r.name).collect();
                    let payload = serde_json::json!({
                        "op": op.as_str(),
                        "project": {
                            "path": palace.project_root().display().to_string(),
                            "existing_rooms": rooms,
                        },
                        "drawers": drawers,
                        "cursor": serde_json::Value::Null,
                    });
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                } else {
                    render_drawer_hits(&drawers, false)?;
                }
                return Ok(());
            }

            let drawers = palace.list_drawers(room, limit, offset)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&drawers)?);
            } else if drawers.is_empty() {
                println!("(no drawers)");
            } else {
                render_drawer_hits(&drawers, false)?;
            }
            Ok(())
        }
        Some("add") => {
            let text = get_positional(
                sub_args,
                &["--room", "--importance", "--source-file", "--source-line"],
            )
            .ok_or_else(|| {
                RecallError::usage(
                    "usage: ndx recall drawer add \"text\" [--room X] [--importance N] [--source-file F]",
                )
            })?;
            let room = get_flag(sub_args, "--room")
                .unwrap_or(recall::UNCLASSIFIED_ROOM)
                .to_string();
            let importance = get_flag_usize(sub_args, "--importance")
                .map(|n| n as u8)
                .unwrap_or(recall::DEFAULT_IMPORTANCE);
            let source_file = get_flag(sub_args, "--source-file").map(|s| s.to_string());
            let source_line =
                get_flag_usize(sub_args, "--source-line").map(|n| n as u32);

            let palace = Palace::open_from_cwd()?;
            let drawer = recall::Drawer {
                id: 0,
                text: text.to_string(),
                content_hash: String::new(),
                room,
                wing: None,
                importance,
                source_kind: recall::SourceKind::Manual,
                source_session_id: None,
                source_file,
                source_line,
                source_commit: None,
                created_at: 0,
                updated_at: 0,
                metadata: std::collections::BTreeMap::new(),
            };
            let outcome = palace.insert_drawer(drawer)?;
            if sub_args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "id": outcome.id, "deduped": outcome.deduped})
                );
            } else if outcome.deduped {
                eprintln!("drawer already existed; bumped importance on id {}", outcome.id);
            } else {
                eprintln!("drawer {} added", outcome.id);
            }
            Ok(())
        }
        Some("update") => {
            let source_file = get_flag(sub_args, "--source-file");
            let search = get_flag(sub_args, "--search");
            let id = get_flag_usize(sub_args, "--id");
            let room = get_flag(sub_args, "--room");
            let importance =
                get_flag_usize(sub_args, "--importance").map(|n| n as u8);
            let text = get_flag(sub_args, "--text");
            let from_room = get_flag(sub_args, "--from-room");
            let dry_run = sub_args.iter().any(|a| a == "--dry-run");

            // Bulk mode: --search <pattern> + --room
            if let Some(pattern) = search {
                let target_room = room.ok_or_else(|| {
                    RecallError::usage(
                        "bulk search update needs --room: ndx recall drawer update --search <pattern> --room <room>",
                    )
                })?;
                let palace = Palace::open_from_cwd()?;
                let (matched, count) = palace.bulk_update_by_search(
                    pattern,
                    target_room,
                    importance,
                    from_room,
                    dry_run,
                )?;
                if sub_args.iter().any(|a| a == "--json") {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": true,
                            "updated": count,
                            "search": pattern,
                            "room": target_room,
                            "dry_run": dry_run,
                        })
                    );
                } else if dry_run {
                    eprintln!(
                        "dry-run: {} drawers would move to room \"{}\"",
                        count, target_room
                    );
                    for d in matched.iter().take(5) {
                        let snippet: String = d.text.chars().take(80).collect::<String>().replace('\n', " ");
                        eprintln!("  [{}] \"{}\"", d.id, snippet);
                    }
                    if count > 5 {
                        eprintln!("  ... ({} more)", count - 5);
                    }
                } else {
                    eprintln!(
                        "updated {} drawers -> room \"{}\" (matched \"{}\")",
                        count, target_room, pattern
                    );
                }
                return Ok(());
            }

            // Bulk mode: --source-file + --room (no --id needed).
            if let Some(sf) = source_file {
                let target_room = room.ok_or_else(|| {
                    RecallError::usage(
                        "bulk update needs --room: ndx recall drawer update --source-file <path> --room <room>",
                    )
                })?;
                let palace = Palace::open_from_cwd()?;
                let count = palace.bulk_update_by_source_file(sf, target_room, importance)?;
                if sub_args.iter().any(|a| a == "--json") {
                    println!(
                        "{}",
                        serde_json::json!({"ok": true, "updated": count, "source_file": sf, "room": target_room})
                    );
                } else {
                    eprintln!(
                        "{} drawers from `{}` moved to room `{}`",
                        count, sf, target_room
                    );
                }
                return Ok(());
            }

            // Single-drawer mode: --id required.
            let id = id.ok_or_else(|| RecallError::usage(
                "usage: ndx recall drawer update --id N [--room X] [--importance N] [--text \"...\"]  OR  --source-file <path> --room <room>",
            ))? as u64;
            if room.is_none() && importance.is_none() && text.is_none() {
                return Err(RecallError::usage(
                    "drawer update needs at least one of --room, --importance, --text",
                )
                .into());
            }
            let palace = Palace::open_from_cwd()?;
            let updated = palace.update_drawer(id, room, importance, text)?;
            if sub_args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "id": updated.id})
                );
            } else {
                eprintln!(
                    "drawer {} updated (room={}, importance={})",
                    updated.id, updated.room, updated.importance
                );
            }
            Ok(())
        }
        Some("rm") => {
            let id = get_flag_usize(sub_args, "--id")
                .ok_or_else(|| RecallError::usage("usage: ndx recall drawer rm --id N"))?
                as u64;
            let palace = Palace::open_from_cwd()?;
            let removed = palace.delete_drawer(id)?;
            if sub_args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::json!({"ok": removed, "id": id})
                );
            } else if removed {
                eprintln!("drawer {} removed", id);
            } else {
                eprintln!("drawer {} not found", id);
            }
            Ok(())
        }
        Some("link") => {
            let from = get_flag_usize(sub_args, "--from")
                .ok_or_else(|| RecallError::usage("usage: ndx recall drawer link --from A --to B --kind <kind>"))? as u64;
            let to = get_flag_usize(sub_args, "--to")
                .ok_or_else(|| RecallError::usage("usage: ndx recall drawer link --from A --to B --kind <kind>"))? as u64;
            let kind_str = get_flag(sub_args, "--kind").ok_or_else(|| {
                RecallError::usage("missing --kind (references|contradicts|supersedes|derived_from)")
            })?;
            let kind = recall::LinkKind::parse(kind_str).ok_or_else(|| {
                RecallError::usage(format!(
                    "unknown link kind `{}`; expected references|contradicts|supersedes|derived_from",
                    kind_str
                ))
            })?;
            let palace = Palace::open_from_cwd()?;
            palace.link_drawers(from, to, kind)?;
            if sub_args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "from": from, "to": to, "kind": kind_str})
                );
            } else {
                eprintln!("linked {} -> {} ({})", from, to, kind_str);
            }
            Ok(())
        }
        Some("unlink") => {
            let from = get_flag_usize(sub_args, "--from")
                .ok_or_else(|| RecallError::usage("usage: ndx recall drawer unlink --from A --to B [--kind <kind>]"))? as u64;
            let to = get_flag_usize(sub_args, "--to")
                .ok_or_else(|| RecallError::usage("usage: ndx recall drawer unlink --from A --to B [--kind <kind>]"))? as u64;
            let kind = match get_flag(sub_args, "--kind") {
                Some(k) => Some(recall::LinkKind::parse(k).ok_or_else(|| {
                    RecallError::usage(format!("unknown link kind `{}`", k))
                })?),
                None => None,
            };
            let palace = Palace::open_from_cwd()?;
            let removed = palace.unlink_drawers(from, to, kind)?;
            if sub_args.iter().any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "from": from, "to": to, "removed": removed})
                );
            } else {
                eprintln!("unlinked {} -> {} ({} link(s))", from, to, removed);
            }
            Ok(())
        }
        Some("show") => {
            let id = get_flag_usize(sub_args, "--id")
                .ok_or_else(|| RecallError::usage("usage: ndx recall drawer show --id N"))?
                as u64;
            let palace = Palace::open_from_cwd()?;
            let drawer = palace.get_drawer(id)?.ok_or_else(|| {
                RecallError::constraint(format!("drawer {} not found", id))
            })?;
            if sub_args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&drawer)?);
            } else {
                println!("id: {}", drawer.id);
                println!("room: {}", drawer.room);
                println!("importance: {}", drawer.importance);
                println!("source_kind: {:?}", drawer.source_kind);
                if let Some(s) = &drawer.source_session_id {
                    println!("source_session_id: {}", s);
                }
                if let Some(f) = &drawer.source_file {
                    println!("source_file: {}", f);
                    if let Some(l) = drawer.source_line {
                        println!("source_line: {}", l);
                    }
                }
                println!("content_hash: {}", drawer.content_hash);
                println!("created_at: {}", format_unix(drawer.created_at));
                println!("updated_at: {}", format_unix(drawer.updated_at));
                if !drawer.metadata.is_empty() {
                    println!("metadata:");
                    for (k, v) in &drawer.metadata {
                        println!("  {}: {}", k, v);
                    }
                }
                println!();
                println!("{}", drawer.text);
            }
            Ok(())
        }
        Some(other) => Err(RecallError::usage(format!(
            "unknown `recall drawer` subcommand `{}`",
            other
        ))
        .into()),
        None => Err(RecallError::usage(
            "usage: ndx recall drawer <list|show>  (more in Phase 6)",
        )
        .into()),
    }
}

fn cmd_recall_wake(args: &[String]) -> Result<()> {
    let palace = Palace::open_from_cwd()?;
    if args.iter().any(|a| a == "--force") {
        let cleared = palace.clear_all_wake_injections()?;
        if cleared > 0 {
            eprintln!(
                "cleared {} session wake-up markers; next Bash hook in each session re-injects",
                cleared
            );
        }
    }
    let text = recall::search::wake_up(&palace)?;
    println!("{}", text);
    Ok(())
}

fn cmd_recall_get(args: &[String]) -> Result<()> {
    let room = get_flag(args, "--room")
        .ok_or_else(|| RecallError::usage("usage: ndx recall get --room <name> [--limit N]"))?;
    let limit = get_flag_usize(args, "--limit").unwrap_or(10);
    let palace = Palace::open_from_cwd()?;
    let drawers = palace.list_drawers(Some(room), limit, 0)?;

    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&drawers)?);
        return Ok(());
    }
    if drawers.is_empty() {
        println!("(no drawers in room `{}`)", room);
        return Ok(());
    }
    // Order by importance desc, updated_at desc (R-522).
    let mut sorted = drawers;
    sorted.sort_by(|a, b| {
        b.importance
            .cmp(&a.importance)
            .then(b.updated_at.cmp(&a.updated_at))
    });
    for d in sorted {
        let snippet: String = d
            .text
            .chars()
            .take(300)
            .collect::<String>()
            .replace('\n', " ");
        println!("[{:>5}] i={}  {}", d.id, d.importance, snippet);
    }
    Ok(())
}

fn cmd_recall_search(args: &[String]) -> Result<()> {
    let query = get_positional(
        args,
        &["--room", "--limit", "--lexical", "--semantic", "--hybrid", "--json"],
    )
    .ok_or_else(|| RecallError::usage("usage: ndx recall search \"query\" [flags]"))?;

    let room = get_flag(args, "--room");
    let limit = get_flag_usize(args, "--limit").unwrap_or(recall::search::DEFAULT_N_OUT);

    let lexical = args.iter().any(|a| a == "--lexical");
    let semantic = args.iter().any(|a| a == "--semantic");
    let hybrid = args.iter().any(|a| a == "--hybrid");
    let mode = match (lexical, semantic, hybrid) {
        (true, false, false) => recall::search::SearchMode::Lexical,
        (false, true, false) => recall::search::SearchMode::Semantic,
        (false, false, _) => recall::search::SearchMode::Hybrid,
        _ => {
            return Err(RecallError::usage(
                "search mode flags --lexical / --semantic / --hybrid are mutually exclusive",
            )
            .into());
        }
    };

    let palace = Palace::open_from_cwd()?;
    let hits = recall::search::search(&palace, query, mode, room, limit)?;

    if args.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&hits)?);
        return Ok(());
    }

    if hits.is_empty() {
        println!("*(no matches)*");
        return Ok(());
    }

    for (i, hit) in hits.iter().enumerate() {
        let sim_str = hit
            .similarity
            .map(|s| format!(" sim={:.3}", s))
            .unwrap_or_default();
        let sem = hit
            .rank_semantic
            .map(|r| format!(" sem#{}", r + 1))
            .unwrap_or_default();
        let lex = hit
            .rank_lexical
            .map(|r| format!(" lex#{}", r + 1))
            .unwrap_or_default();
        println!(
            "[{}] [{}] i={} score={:.4}{}{}{}",
            i + 1,
            hit.drawer.room,
            hit.drawer.importance,
            hit.score,
            sim_str,
            sem,
            lex,
        );
        let snippet: String = hit
            .drawer
            .text
            .chars()
            .take(300)
            .collect::<String>()
            .replace('\n', " ");
        println!("    id={}  {}", hit.drawer.id, snippet);
        if let Some(f) = &hit.drawer.source_file {
            println!("    src: {}", f);
        } else if let Some(s) = &hit.drawer.source_session_id {
            println!("    session: {}", recall::safe_prefix(s, 8));
        }
    }
    Ok(())
}

fn cmd_recall_reembed(args: &[String]) -> Result<()> {
    let palace = Palace::open_from_cwd()?;
    let force = args.iter().any(|a| a == "--force");
    let count = palace.reembed_all(force)?;
    eprintln!("reembedded {} drawers", count);
    Ok(())
}

fn cmd_recall_link_palace(args: &[String]) -> Result<()> {
    cmd_recall_link_palace_at(args, &recall::current_project_root()?)
}

/// Extracted so tests can exercise the command against a temp dir
/// without depending on CWD.
fn cmd_recall_link_palace_at(args: &[String], cwd: &std::path::Path) -> Result<()> {
    let target_root = get_positional(args, &[])
        .ok_or_else(|| RecallError::usage("usage: ndx recall link-palace <canonical-root> [--force]"))?;
    let force = args.iter().any(|a| a == "--force");

    let target_abs = recall::absolute_path(std::path::Path::new(target_root));
    let target_db = target_abs.join(".ndx").join("recall.redb");
    if !target_db.exists() {
        // R-1041: refuse if the target palace does not exist.
        return Err(RecallError::constraint(format!(
            "target palace does not exist: {}",
            target_db.display()
        ))
        .into());
    }

    let ndx_dir = cwd.join(".ndx");
    std::fs::create_dir_all(&ndx_dir).with_context(|| {
        format!("creating {}", ndx_dir.display())
    })?;
    let local_db = ndx_dir.join("recall.redb");

    // R-1042: refuse if the local palace has drawers, unless --force.
    let local_is_symlink = std::fs::symlink_metadata(&local_db)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if local_db.exists() && !local_is_symlink {
        let palace = Palace::open_at(cwd.to_path_buf())?;
        let stats = palace.stats()?;
        drop(palace);
        if stats.drawer_count > 0 && !force {
            return Err(RecallError::constraint(format!(
                "refusing to replace palace with {} drawers; pass --force to overwrite",
                stats.drawer_count
            ))
            .into());
        }
        std::fs::remove_file(&local_db).with_context(|| {
            format!("removing {}", local_db.display())
        })?;
    } else if local_is_symlink {
        std::fs::remove_file(&local_db).with_context(|| {
            format!("removing symlink {}", local_db.display())
        })?;
    }

    let final_target = resolve_one_hop_symlink(&target_db)?;
    std::os::unix::fs::symlink(&final_target, &local_db).with_context(|| {
        format!(
            "creating symlink {} -> {}",
            local_db.display(),
            final_target.display()
        )
    })?;
    install::ensure_gitignore_entry(cwd)?;
    eprintln!(
        "recall palace linked: {} -> {}",
        local_db.display(),
        final_target.display()
    );
    Ok(())
}

fn cmd_recall_unlink_palace(args: &[String]) -> Result<()> {
    cmd_recall_unlink_palace_at(args, &recall::current_project_root()?)
}

fn cmd_recall_unlink_palace_at(args: &[String], cwd: &std::path::Path) -> Result<()> {
    let keep = args.iter().any(|a| a == "--keep");
    let local_db = cwd.join(".ndx").join("recall.redb");
    let meta = std::fs::symlink_metadata(&local_db)
        .map_err(|e| RecallError::constraint(format!(
            "cannot stat {}: {}", local_db.display(), e
        )))?;
    if !meta.file_type().is_symlink() {
        return Err(RecallError::constraint(format!(
            "{} is not a symlink — nothing to unlink",
            local_db.display()
        ))
        .into());
    }

    if keep {
        // R-1052: MVCC read-txn copy of the canonical palace into a
        // staging file, then atomic rename.
        let palace = Palace::open_at(cwd.to_path_buf())?;
        let staging = cwd.join(".ndx").join("recall.redb.new");
        if staging.exists() {
            std::fs::remove_file(&staging).ok();
        }
        palace.mvcc_copy_to(&staging)?;
        drop(palace);
        std::fs::remove_file(&local_db).with_context(|| {
            format!("removing symlink {}", local_db.display())
        })?;
        std::fs::rename(&staging, &local_db).with_context(|| {
            format!("renaming {} -> {}", staging.display(), local_db.display())
        })?;
        eprintln!(
            "palace unlinked and copied locally: {}",
            local_db.display()
        );
    } else {
        std::fs::remove_file(&local_db).with_context(|| {
            format!("removing symlink {}", local_db.display())
        })?;
        eprintln!("palace symlink removed: {}", local_db.display());
    }
    Ok(())
}

fn cmd_recall_rehome(args: &[String]) -> Result<()> {
    let new_root = get_positional(args, &[])
        .ok_or_else(|| RecallError::usage("usage: ndx recall rehome <new-canonical-root>"))?;
    let new_abs = recall::absolute_path(std::path::Path::new(new_root));
    let palace = Palace::open_from_cwd()?;
    palace.set_canonical_root(&new_abs)?;
    eprintln!("canonical_root rewritten to {}", new_abs.display());
    Ok(())
}

fn cmd_recall_rebuild_index(_args: &[String]) -> Result<()> {
    let root = Palace::find()?
        .ok_or_else(|| anyhow::Error::from(RecallError::not_initialized()))?;
    let palace = Palace::open_for_migration(root)?;
    let count = palace.rebuild_bm25_index()?;
    eprintln!("rebuilt BM25 index for {} drawers", count);
    Ok(())
}

fn format_unix(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

// ── Main ──

fn main() {
    std::process::exit(run_main());
}

fn run_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json_mode = args.iter().any(|a| a == "--json");
    let result = dispatch(&args);
    match result {
        Ok(()) => 0,
        Err(e) => {
            if let Some(re) = e.downcast_ref::<RecallError>() {
                if json_mode {
                    // R-1002: structured error envelope on stdout.
                    let payload = serde_json::json!({
                        "ok": false,
                        "error": re.message,
                        "code": re.code.as_i32(),
                    });
                    println!("{}", payload);
                } else {
                    eprintln!("{}", re.message);
                }
                re.code.as_i32()
            } else {
                if json_mode {
                    let payload = serde_json::json!({
                        "ok": false,
                        "error": format!("{:#}", e),
                        "code": 1,
                    });
                    println!("{}", payload);
                } else {
                    eprintln!("Error: {:#}", e);
                }
                1
            }
        }
    }
}

fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        // Index commands (via daemon)
        Some("search") => cmd_search(&args[1..]),
        Some("list") => cmd_list(&args[1..]),
        Some("find") => cmd_find(&args[1..]),
        Some("status") => cmd_status(),

        // Memory commands (direct)
        Some("memory") => cmd_memory(&args[1..]),

        // Recall commands (direct palace access)
        Some("recall") => cmd_recall(&args[1..]),

        // Issue tracker (drawers in `_issues_` room with status / milestone meta)
        Some("issue") => cmd_issue(&args[1..]),

        // Cross-reference commands (direct memory + filesystem)
        Some("xref") => cmd_xref(&args[1..]),

        // Daemon control
        Some("stop") => cmd_stop(),
        Some("ping") => cmd_ping(),
        Some("daemon") => {
            let root = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cmd_daemon(root))
        }

        // Hook/filter
        Some("hook") => cmd_hook(),
        Some("filter") => {
            let key = args.get(1).map(|s| s.as_str()).unwrap_or("unknown");
            cmd_filter(key)
        }

        // Maintenance
        Some("scan") => cmd_scan(),
        Some("install") => install::run_install(),
        Some("init") => {
            let mut clean_up = false;
            let mut dir: Option<PathBuf> = None;
            for a in &args[1..] {
                match a.as_str() {
                    "--clean-up" => clean_up = true,
                    other if !other.starts_with("--") && dir.is_none() => {
                        dir = Some(PathBuf::from(other));
                    }
                    other => anyhow::bail!("unknown argument to init: {}", other),
                }
            }
            cmd_init(dir.unwrap_or_else(|| PathBuf::from(".")), clean_up)
        }

        Some("--version" | "-V" | "version") => {
            println!("ndx {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help" | "--help" | "-h") => {
            print_usage();
            Ok(())
        }
        _ => {
            print_usage();
            Ok(())
        }
    }
}

#[cfg(test)]
mod precompact_tests {
    use super::*;
    use recall::{Drawer, SourceKind};
    use std::collections::BTreeMap;

    fn mk_drawer(text: &str, importance: u8, room: &str) -> Drawer {
        Drawer {
            id: 0,
            text: text.to_string(),
            content_hash: String::new(),
            room: room.to_string(),
            wing: None,
            importance,
            source_kind: SourceKind::Manual,
            source_session_id: None,
            source_file: None,
            source_line: None,
            source_commit: None,
            created_at: 0,
            updated_at: 0,
            metadata: BTreeMap::new(),
        }
    }

    /// Build a minimal palace at a temp dir with enough content to make
    /// `wake_up()` return non-trivial text.
    fn make_palace(tmp: &std::path::Path) -> Palace {
        let palace = Palace::create_at(tmp.to_path_buf()).unwrap();
        palace.ensure_room("architecture", None, None).unwrap();
        palace
            .insert_drawer_no_embedding(mk_drawer(
                "Test project uses Rust edition 2021 and redb for storage.",
                8,
                "architecture",
            ))
            .unwrap();
        palace
    }

    #[test]
    fn precompact_returns_none_without_cwd() {
        let hi = hook::HookInput {
            session_id: Some("abc123".into()),
            cwd: None,
            tool_name: None,
            tool_input: None,
            hook_event_name: Some("PreCompact".into()),
            trigger: Some("manual".into()),
            custom_instructions: None,
            source: None,
            reason: None,
            transcript_path: None,
        };
        let out = build_precompact_output(&hi).unwrap();
        assert!(out.is_none(), "missing cwd must soft-fail with None");
    }

    #[test]
    fn precompact_returns_none_when_no_palace() {
        let tmp = tempfile::tempdir().unwrap();
        let hi = hook::HookInput {
            session_id: Some("abc123".into()),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            tool_name: None,
            tool_input: None,
            hook_event_name: Some("PreCompact".into()),
            trigger: Some("auto".into()),
            custom_instructions: None,
            source: None,
            reason: None,
            transcript_path: None,
        };
        let out = build_precompact_output(&hi).unwrap();
        assert!(out.is_none(), "no palace must soft-fail with None");
    }

    #[test]
    fn precompact_emits_wake_up_block() {
        let tmp = tempfile::tempdir().unwrap();
        // Drop the palace before invoking the handler — redb holds an
        // exclusive lock on the db file while the handle is alive.
        drop(make_palace(tmp.path()));

        let hi = hook::HookInput {
            session_id: Some("sess-pre-compact-0001".into()),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            tool_name: None,
            tool_input: None,
            hook_event_name: Some("PreCompact".into()),
            trigger: Some("manual".into()),
            custom_instructions: Some("".into()),
            source: None,
            reason: None,
            transcript_path: None,
        };
        let out = build_precompact_output(&hi).unwrap().expect("output");

        // JSON serialises to the exact shape Claude Code expects.
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("\"hookEventName\":\"PreCompact\""),
            "event name must be PreCompact in JSON: {}",
            json
        );
        assert!(
            json.contains("\"additionalContext\""),
            "must carry additionalContext: {}",
            json
        );
        // Must NOT emit PreToolUse-only fields.
        assert!(
            !json.contains("permissionDecision"),
            "PreCompact output must not include permissionDecision: {}",
            json
        );
        assert!(
            !json.contains("updatedInput"),
            "PreCompact output must not include updatedInput: {}",
            json
        );

        let body = out
            .hook_specific_output
            .additional_context
            .as_deref()
            .unwrap();
        assert!(body.contains("ndx-recall wake-up"));
        assert!(body.contains("pre-compact"));
        assert!(body.contains("trigger=manual"));
        // Session id prefix (8 chars) appears.
        assert!(body.contains("sess-pre"));
        // The wake-up content itself (one of the L1 or L0 markers).
        assert!(body.contains("L1") || body.contains("L0") || body.contains("Rust"));
    }

    /// PreCompact does NOT consult the per-session WAKE_INJECTED gate.
    /// Running it twice for the same session must still produce output
    /// both times.
    #[test]
    fn precompact_ignores_wake_injected_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let palace = make_palace(tmp.path());
        // Pretend PreToolUse already injected for this session.
        palace
            .mark_wake_injected("already-seen-session")
            .unwrap();
        drop(palace);

        let hi = hook::HookInput {
            session_id: Some("already-seen-session".into()),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            tool_name: None,
            tool_input: None,
            hook_event_name: Some("PreCompact".into()),
            trigger: Some("auto".into()),
            custom_instructions: None,
            source: None,
            reason: None,
            transcript_path: None,
        };
        let out1 = build_precompact_output(&hi).unwrap();
        let out2 = build_precompact_output(&hi).unwrap();
        assert!(out1.is_some(), "first call must emit");
        assert!(out2.is_some(), "second call must still emit");
    }

    // ── SessionStart ─────────────────────────────────────────────────

    fn mk_unclassified(text: &str) -> Drawer {
        let mut d = mk_drawer(text, recall::DEFAULT_IMPORTANCE, recall::UNCLASSIFIED_ROOM);
        // Mark as memory-mined so list_pending(Score) considers it.
        d.source_kind = SourceKind::Memory;
        d
    }

    /// Insert N drawers into the unclassified room with default importance.
    /// They count toward both `classify` and `score` pending queues
    /// (source_kind != Manual).
    fn fill_unclassified(palace: &Palace, n: usize) {
        for i in 0..n {
            let d = mk_unclassified(&format!("pending fragment number {} for hygiene tests", i));
            palace.insert_drawer_no_embedding(d).unwrap();
        }
    }

    #[test]
    fn session_start_hook_empty_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let palace = Palace::create_at(tmp.path().to_path_buf()).unwrap();
        // Far below the 20-drawer threshold (each unclassified drawer
        // counts toward both classify and score, so 5 → 10 total).
        fill_unclassified(&palace, 5);

        let out = session_start_nudge_for(&palace).unwrap();
        assert!(
            out.is_none(),
            "below threshold must produce no additionalContext"
        );
    }

    #[test]
    fn session_start_hook_emits_nudge_when_above_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let palace = Palace::create_at(tmp.path().to_path_buf()).unwrap();
        // 25 unclassified → 25 classify + 25 score = 50 ≥ 20.
        fill_unclassified(&palace, 25);

        let out = session_start_nudge_for(&palace)
            .unwrap()
            .expect("must emit when backlog crosses threshold");
        let body = out
            .hook_specific_output
            .additional_context
            .as_deref()
            .unwrap();
        assert!(
            body.contains("Run `/ndx-chore`"),
            "nudge must invite /ndx-chore: {}",
            body
        );
        assert!(
            body.contains("ndx-recall — palace hygiene pending"),
            "nudge must use the agreed header: {}",
            body
        );
    }

    #[test]
    fn session_start_hook_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let palace = Palace::create_at(tmp.path().to_path_buf()).unwrap();
        fill_unclassified(&palace, 25);

        let out = session_start_nudge_for(&palace).unwrap().expect("output");
        let json = serde_json::to_string(&out).unwrap();

        // Must mirror the PreCompact JSON shape: hookSpecificOutput with
        // hookEventName + additionalContext, no permissionDecision.
        assert!(
            json.contains("\"hookSpecificOutput\""),
            "json: {}",
            json
        );
        assert!(
            json.contains("\"hookEventName\":\"SessionStart\""),
            "json: {}",
            json
        );
        assert!(json.contains("\"additionalContext\""), "json: {}", json);
        assert!(
            !json.contains("permissionDecision"),
            "SessionStart must not emit permissionDecision: {}",
            json
        );
        assert!(
            !json.contains("updatedInput"),
            "SessionStart must not emit updatedInput: {}",
            json
        );
    }

    // ── SessionEnd ───────────────────────────────────────────────────

    #[test]
    fn session_end_hook_soft_fails_without_palace() {
        let tmp = tempfile::tempdir().unwrap();
        // No palace at this dir.
        let hi = hook::HookInput {
            session_id: Some("sess-end-1".into()),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            tool_name: None,
            tool_input: None,
            hook_event_name: Some("SessionEnd".into()),
            trigger: None,
            custom_instructions: None,
            source: None,
            reason: Some("other".into()),
            transcript_path: None,
        };
        // Must not panic / error — soft-fail.
        handle_session_end(&hi).unwrap();
    }

    #[test]
    fn session_end_hook_soft_fails_without_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        drop(make_palace(tmp.path()));
        let hi = hook::HookInput {
            session_id: None,
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            tool_name: None,
            tool_input: None,
            hook_event_name: Some("SessionEnd".into()),
            trigger: None,
            custom_instructions: None,
            source: None,
            reason: Some("other".into()),
            transcript_path: None,
        };
        handle_session_end(&hi).unwrap();
    }

    /// `mine_from_memory_with_opts` with a `session_ids` filter that
    /// matches no session in global memory must return Ok with zero
    /// added drawers — i.e. the SessionEnd hook idempotently no-ops
    /// when the just-ended session was never recorded in memory.redb.
    /// This is the closest we can get to an end-to-end test without
    /// stubbing out the global memory database in a non-test process.
    #[test]
    fn session_end_mine_filter_no_match_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let palace = Palace::create_at(tmp.path().to_path_buf()).unwrap();

        let mut allow = std::collections::HashSet::new();
        allow.insert("nonexistent-session-id-xyz-9999".to_string());

        let report = recall::mine::mine_from_memory_with_opts(
            &palace,
            recall::mine::MineFromMemoryOpts {
                since: None,
                force: false,
                embed: false,
                session_ids: Some(allow),
            },
        )
        .unwrap();
        assert_eq!(report.added, 0, "no matching session → zero added");
        assert_eq!(report.deduped, 0);
    }
}
