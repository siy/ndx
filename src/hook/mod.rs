pub mod context;
pub mod filter;
pub mod manifest;
pub mod parser;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<ToolInput>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    // PreCompact-only fields; harmless for other events.
    #[serde(default)]
    pub trigger: Option<String>,
    // `custom_instructions` is part of the PreCompact schema but is
    // not consumed today. Keep the field deserialized (so unknown
    // fields don't trip us up if we ever flip serde's deny_unknown)
    // but mark it allow-dead-code until a use-case arrives.
    #[serde(default)]
    #[allow(dead_code)]
    pub custom_instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolInput {
    pub command: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookOutput {
    pub hook_specific_output: HookSpecificOutput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookSpecificOutput {
    pub hook_event_name: String,
    pub permission_decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<UpdatedInput>,
}

#[derive(Debug, Serialize)]
pub struct UpdatedInput {
    pub command: String,
}

// ── PreCompact output ────────────────────────────────────────────────
//
// PreCompact's `hookSpecificOutput` shape is narrower than PreToolUse —
// no `permissionDecision`, no `updatedInput`, just `hookEventName` and
// (optionally) `additionalContext`. Using a distinct struct keeps the
// JSON clean and avoids emitting irrelevant fields that Claude Code
// might reject or warn about.

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreCompactOutput {
    pub hook_specific_output: PreCompactSpecificOutput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreCompactSpecificOutput {
    pub hook_event_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

pub fn handle_hook(stdin_json: &str) -> Result<Option<HookOutput>> {
    let input: HookInput = serde_json::from_str(stdin_json)?;

    // Only handle Bash tool calls
    if input.tool_name.as_deref() != Some("Bash") {
        return Ok(None);
    }

    let command = match input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.command.as_deref())
    {
        Some(cmd) => cmd,
        None => return Ok(None),
    };

    let parsed = match parser::parse_command(command) {
        Some(p) => p,
        None => return Ok(None),
    };

    // Look up manifest
    let cwd = input.cwd.as_deref();
    let manifest = match manifest::resolve_manifest(&parsed.key, cwd) {
        Some(m) => m,
        None => {
            // Try simple key if compound key didn't match
            if parsed.subcommand.is_some() {
                match manifest::resolve_manifest(&parsed.cmd, cwd) {
                    Some(m) => m,
                    None => return Ok(None),
                }
            } else {
                return Ok(None);
            }
        }
    };

    // Check platform
    if !manifest::platform_matches(&manifest.platform) {
        return Ok(None);
    }

    // Phase A: Build additionalContext
    let additional_context = context::build_context(&manifest);

    // Phase B: Build updatedInput if filter is enabled
    let ndx_bin = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ndx".to_string());

    let updated_input = if manifest
        .output_schema
        .as_ref()
        .map_or(false, |os| os.enable_filter)
        && filter::is_filterable(command)
    {
        Some(UpdatedInput {
            command: format!("{} | \"{}\" filter {}", command, ndx_bin, parsed.key),
        })
    } else {
        None
    };

    Ok(Some(HookOutput {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PreToolUse".to_string(),
            permission_decision: "allow".to_string(),
            additional_context: Some(additional_context),
            updated_input,
        },
    }))
}
