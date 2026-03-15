use super::manifest::CommandManifest;

pub fn build_context(manifest: &CommandManifest) -> String {
    let mut ctx = String::new();

    // Header
    let cmd_label = if let Some(ref sub) = manifest.subcommand {
        format!("{} {}", manifest.command, sub)
    } else {
        manifest.command.clone()
    };
    ctx.push_str(&format!("[ndx] {}: {}\n", cmd_label, manifest.description));

    if let Some(ref syntax) = manifest.syntax {
        // Usage
        if let Some(ref usage) = syntax.usage {
            ctx.push_str(&format!("Usage: {}\n", usage));
        }

        // Key flags (up to 5)
        if !syntax.key_flags.is_empty() {
            ctx.push_str("Key flags:\n");
            for flag in syntax.key_flags.iter().take(5) {
                if let Some(ref use_when) = flag.use_when {
                    ctx.push_str(&format!(
                        "  {}: {}  \u{2192} {}\n",
                        flag.flag, flag.description, use_when
                    ));
                } else {
                    ctx.push_str(&format!("  {}: {}\n", flag.flag, flag.description));
                }
            }
        }

        // Preferred invocations (up to 3)
        if !syntax.preferred_invocations.is_empty() {
            ctx.push_str("Prefer:\n");
            for inv in syntax.preferred_invocations.iter().take(3) {
                ctx.push_str(&format!("  {}  # {}\n", inv.invocation, inv.use_when));
            }
        }
    }

    // Auto-generated notice
    if manifest.generated == Some(true) {
        ctx.push_str("(auto-generated manifest -- improve at ~/.kcp/commands/)\n");
    }

    ctx
}
