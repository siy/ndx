mod client;
mod daemon;
mod hook;
mod index;
mod install;
mod memory;
mod recall;
mod scanner;
mod server;
mod trigram;
mod watcher;

use anyhow::{Context, Result};
use memory::MemoryIndex;
use recall::{ExitCode, Palace, RecallError};
use std::path::PathBuf;

fn print_usage() {
    eprintln!("ndx — Fast File Index & Memory Search CLI");
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
    eprintln!("  ndx xref file <path>     Find sessions that touched a file");
    eprintln!("  ndx xref session <id>    List files touched by a session");
    eprintln!("  --limit N                Limit results");
    eprintln!();
    eprintln!("Daemon commands:");
    eprintln!("  ndx stop                 Stop the background daemon");
    eprintln!("  ndx ping                 Check if daemon is running");
    eprintln!();
    eprintln!("Other commands:");
    eprintln!("  ndx scan                 Scan memory (sessions, events, agents)");
    eprintln!("  ndx hook                 PreToolUse hook handler (stdin/stdout)");
    eprintln!("  ndx filter <key>         Output noise filter (stdin/stdout)");
    eprintln!("  ndx install              Download manifests, register hook + skill");
    eprintln!("  ndx init [path]          Install ndx skill into a project");
    eprintln!("  ndx help                 Show this help message");
}

// ── Argument parsing helpers ──

fn get_flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
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
    let sub_args = if args.len() > 1 { &args[1..] } else { &[] };
    let limit = get_flag_usize(args, "--limit");

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

// ── Hook/filter commands ──

fn cmd_hook() -> Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;

    match hook::handle_hook(&input) {
        Ok(Some(response)) => {
            println!("{}", serde_json::to_string(&response)?);
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("[ndx hook] error: {}", e);
        }
    }

    // Phase C: log event to memory (best-effort)
    if let Ok(hook_input) = serde_json::from_str::<hook::HookInput>(&input) {
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
                    command: command[..command.len().min(500)].to_string(),
                    manifest_key,
                    ingested_at: chrono::Utc::now().to_rfc3339(),
                };
                let _ = mem.insert_event(&entry);
            }
        }
    }

    Ok(())
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

fn cmd_init(dir: PathBuf) -> Result<()> {
    let dir = dir.canonicalize().context("invalid directory path")?;
    install::install_skill_to_project(&dir)?;
    eprintln!(
        "ndx skill installed to {}/.claude/commands/ndx.md",
        dir.display()
    );
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
        Some("init") => cmd_recall_init(),
        Some("status") => cmd_recall_status(sub_args),
        Some("room") => cmd_recall_room(sub_args),
        Some("identity") => cmd_recall_identity(sub_args),
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
    eprintln!("  ndx recall init                 Create .ndx/recall.redb in current project");
    eprintln!("  ndx recall status [--json]      Palace statistics");
    eprintln!();
    eprintln!("Rooms:");
    eprintln!("  ndx recall room add <name> [--title T] [--description D]");
    eprintln!("  ndx recall room list [--json]");
    eprintln!("  ndx recall room show <name> [--json]");
    eprintln!("  ndx recall room rm <name>");
    eprintln!("  ndx recall room rename <old> <new>");
    eprintln!();
    eprintln!("Identity:");
    eprintln!("  ndx recall identity show [--merged]");
    eprintln!("  ndx recall identity edit [--project]");
}

fn cmd_recall_init() -> Result<()> {
    let root = recall::current_project_root()?;
    let _palace = Palace::create_at(root.clone())?;
    eprintln!(
        "recall palace initialized at {}/.ndx/recall.redb",
        root.display()
    );
    Ok(())
}

fn cmd_recall_status(args: &[String]) -> Result<()> {
    let palace = Palace::open_from_cwd()?;
    let stats = palace.stats()?;
    let json = args.iter().any(|a| a == "--json");
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }
    println!("Recall palace: {}", palace.db_path().display());
    println!("  Schema version: {}", stats.schema_version);
    println!("  Drawers: {}", stats.drawer_count);
    println!("  Rooms:   {}", stats.room_count);
    println!("  Links:   {}", stats.link_count);
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
    let result = dispatch(&args);
    match result {
        Ok(()) => 0,
        Err(e) => {
            if let Some(re) = e.downcast_ref::<RecallError>() {
                eprintln!("{}", re.message);
                re.code.as_i32()
            } else {
                eprintln!("Error: {:#}", e);
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
            let dir = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            cmd_init(dir)
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
