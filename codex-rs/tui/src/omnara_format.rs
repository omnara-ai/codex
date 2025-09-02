use codex_core::protocol::{FileChange, McpInvocation};
use mcp_types::CallToolResult;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Format patch changes for display in Omnara dashboard.
/// Returns (details_markdown, added_lines, removed_lines).
pub fn format_patch_details(changes: &HashMap<PathBuf, FileChange>) -> (String, usize, usize) {
    let mut patch_details = String::new();
    let mut added_lines = 0usize;
    let mut removed_lines = 0usize;
    const MAX_DIFF_LINES: usize = 100;

    for (path, change) in changes {
        let path_str = path.display().to_string();

        if !patch_details.is_empty() {
            patch_details.push_str("\n");
        }

        match change {
            FileChange::Add { content } => {
                added_lines += content.lines().count();
                patch_details.push_str(&format!("**New file: {}**\n", path_str));
                patch_details.push_str("```diff\n");
                let total = content.lines().count();
                for line in content.lines().take(MAX_DIFF_LINES) {
                    patch_details.push_str(&format!("+{}\n", line));
                }
                if total > MAX_DIFF_LINES {
                    patch_details.push_str(&format!("... ({} more lines)\n", total - MAX_DIFF_LINES));
                }
                patch_details.push_str("```\n");
            }
            FileChange::Update { unified_diff, .. } => {
                patch_details.push_str(&format!("**{}**\n", path_str));
                patch_details.push_str("```diff\n");
                let total = unified_diff.lines().count();
                for line in unified_diff.lines().take(MAX_DIFF_LINES) {
                    patch_details.push_str(line);
                    patch_details.push('\n');
                }
                if total > MAX_DIFF_LINES {
                    patch_details.push_str(&format!("... ({} more lines)\n", total - MAX_DIFF_LINES));
                }
                patch_details.push_str("```\n");

                for line in unified_diff.lines() {
                    if line.starts_with('+') && !line.starts_with("+++") {
                        added_lines += 1;
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        removed_lines += 1;
                    }
                }
            }
            FileChange::Delete => {
                removed_lines += 1; // heuristic placeholder
                patch_details.push_str(&format!("**Delete file: {}**\n", path_str));
            }
        }
    }

    (patch_details, added_lines, removed_lines)
}

/// Build a complete non-approval Omnara note for a patch apply event.
/// Includes a summary line, a file list, and formatted diff details.
pub fn format_patch_note(changes: &HashMap<PathBuf, FileChange>) -> String {
    let file_count = changes.len();
    let (details, added, removed) = format_patch_details(changes);

    let mut msg = String::new();
    use std::fmt::Write as _;
    let _ = write!(
        &mut msg,
        "✏️ Applying patch to {} file{} (+{} -{})\n",
        file_count,
        if file_count == 1 { "" } else { "s" },
        added,
        removed
    );
    for path in changes.keys() {
        let _ = write!(&mut msg, "  └ {}\n", path.display());
    }
    if !details.is_empty() {
        msg.push_str("\n");
        msg.push_str(&details);
    }
    msg
}

/// Build a concise, styled Omnara note for an executed command, with a trimmed output preview.
pub fn format_exec_note(command: &[String], output: &crate::history_cell::CommandOutput) -> String {
    let cmd_str = command.join(" ");
    let ok = output.exit_code == 0;
    let status = if ok {
        "Success".to_string()
    } else {
        format!("Failed (exit {})", output.exit_code)
    };

    let mut msg = format!("**Exec:** `{}`\n**Status:** {}", cmd_str, status);

    // Build a trimmed preview: up to N lines, M chars per line, and K total chars.
    const MAX_LINES: usize = 20;
    const MAX_LINE_CHARS: usize = 200;
    const MAX_TOTAL_CHARS: usize = 2000;
    let mut preview = String::new();
    let mut shown_lines = 0usize;
    let mut total_chars = 0usize;
    let mut truncated_by_chars = false;
    for raw_line in output.formatted_output.lines() {
        if shown_lines >= MAX_LINES { break; }
        // Clip each line to MAX_LINE_CHARS
        let mut line = raw_line.to_string();
        if line.chars().count() > MAX_LINE_CHARS {
            line = line.chars().take(MAX_LINE_CHARS).collect::<String>();
            line.push_str(" …");
        }
        let line_len = line.len() + 1; // include newline
        if total_chars + line_len > MAX_TOTAL_CHARS {
            truncated_by_chars = true;
            break;
        }
        preview.push_str(&line);
        preview.push('\n');
        total_chars += line_len;
        shown_lines += 1;
    }
    if !preview.trim().is_empty() {
        msg.push_str("\n\n```text\n");
        msg.push_str(&preview);
        let total_lines = output.formatted_output.lines().count();
        if truncated_by_chars || shown_lines < total_lines {
            msg.push_str("… (truncated)\n");
        }
        msg.push_str("```");
    }
    msg
}

/// Format an MCP tool call begin note.
pub fn format_mcp_begin_note(invocation: &McpInvocation) -> String {
    let inv = format_mcp_invocation(invocation);
    format!("**Tool:** {}\n**Status:** Running", inv)
}

/// Format an MCP tool call end note.
pub fn format_mcp_end_note(
    invocation: &McpInvocation,
    result: &Result<CallToolResult, String>,
    _duration: std::time::Duration,
) -> String {
    let inv = format_mcp_invocation(invocation);
    let ok = match result {
        Ok(r) => !r.is_error.unwrap_or(false),
        Err(_) => false,
    };
    let status = if ok { "Success" } else { "Failed" };
    format!("**Tool:** {}\n**Status:** {}", inv, status)
}

fn format_mcp_invocation(invocation: &McpInvocation) -> String {
    let args_str = invocation
        .arguments
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_default();
    if args_str.is_empty() {
        format!("{}.{}", invocation.server, invocation.tool)
    } else {
        format!("{}.{}({})", invocation.server, invocation.tool, args_str)
    }
}

/// Format an exec approval request message with command and options.
pub fn format_exec_approval_request(command: &[String], reason: Option<&str>) -> String {
    let command_str = command.join(" ");
    let reason_str = reason.unwrap_or("Agent wants to execute a command");
    format!(
        "**Execute command?**\n\n{}\n\n```bash\n{}\n```\n\n[OPTIONS]\n1. Yes\n2. Always\n3. No, provide feedback\n[/OPTIONS]",
        reason_str, command_str
    )
}

/// Format a patch approval request message with optional reason, grant root, and details.
pub fn format_patch_approval_request(
    file_count: usize,
    added_lines: usize,
    removed_lines: usize,
    reason: Option<&str>,
    grant_root: Option<&Path>,
    patch_details: Option<&str>,
) -> String {
    let mut approval_msg = format!(
        "**Proposed patch to {} file{} (+{} -{})**",
        file_count,
        if file_count == 1 { "" } else { "s" },
        added_lines,
        removed_lines
    );
    if let Some(root) = grant_root {
        approval_msg.push_str(&format!(
            "\n\nThis will grant write access to {} for the remainder of this session.",
            root.display()
        ));
    }
    if let Some(r) = reason {
        approval_msg.push_str(&format!("\n\n{}", r));
    }
    if let Some(details) = patch_details {
        if !details.is_empty() {
            approval_msg.push_str("\n\n");
            approval_msg.push_str(details);
        }
    }
    approval_msg.push_str(
        "\n\n**Apply changes?**\n\n[OPTIONS]\n1. Yes\n2. No, provide feedback\n[/OPTIONS]",
    );
    approval_msg
}
