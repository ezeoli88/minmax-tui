pub mod ask_user;
pub mod bash;
pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod lint;
pub mod list_dir;
pub mod read_file;
pub mod sub_agent;
pub mod todo_write;
pub mod web_fetch;
pub mod web_search;
pub mod write_file;

use crate::core::Mode;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

/// Returns a brief system reminder to append to tool results.
/// These reminders reinforce key instructions from the system prompt and help
/// combat "lost in the middle" attention decay in long conversations.
pub fn system_reminder(mode: Mode, tool_name: &str) -> String {
    let mode_hint = match mode {
        Mode::Plan => "Mode: PLAN (read-only). You cannot write/edit/run commands.",
        Mode::Builder => "Mode: BUILDER. Always read_file before edit_file.",
    };

    let tool_hint = match tool_name {
        "edit_file" | "write_file" => " Verify the change is correct before proceeding.",
        "bash" => " Check exit code. Use dedicated tools (read_file, glob, grep) over bash when possible.",
        "read_file" => "",
        "glob" | "grep" => " Use read_file to examine matches before editing.",
        "web_search" | "web_fetch" => "",
        "sub_agent" => " Review sub-agent findings carefully.",
        _ => "",
    };

    format!(
        "\n\n<system-reminder>{}{} Batch ask_user questions. Use todo_write for multi-step tasks.</system-reminder>",
        mode_hint, tool_hint
    )
}

/// Metadata about a tool execution result, used for rich UI rendering.
#[derive(Debug, Clone)]
pub enum ToolResultMeta {
    EditFile {
        path: String,
        old_str: String,
        new_str: String,
    },
    WriteFile {
        path: String,
        content: String,
        is_new: bool,
    },
}

/// Result from executing a tool.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub result: String,
    pub meta: Option<ToolResultMeta>,
}

impl ToolExecutionResult {
    pub fn text(result: String) -> Self {
        Self { result, meta: None }
    }

    pub fn with_meta(result: String, meta: ToolResultMeta) -> Self {
        Self {
            result,
            meta: Some(meta),
        }
    }
}

static READ_ONLY_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s = HashSet::new();
    s.insert("read_file");
    s.insert("glob");
    s.insert("grep");
    s.insert("list_directory");
    s.insert("web_search");
    s.insert("web_fetch");
    s.insert("ask_user");
    s.insert("todo_write");
    s
});

/// Returns all tool definitions as JSON (OpenAI function calling format).
pub fn get_tool_definitions(mode: Mode) -> Vec<Value> {
    let all = vec![
        bash::definition(),
        read_file::definition(),
        write_file::definition(),
        edit_file::definition(),
        glob::definition(),
        grep::definition(),
        list_dir::definition(),
        web_search::definition(),
        web_fetch::definition(),
        ask_user::definition(),
        todo_write::definition(),
    ];

    match mode {
        Mode::Builder => all,
        Mode::Plan => {
            let mut plan_tools: Vec<Value> = all
                .into_iter()
                .filter(|d| {
                    let name = d
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    READ_ONLY_TOOLS.contains(name)
                })
                .collect();
            // sub_agent is only available in PLAN mode
            plan_tools.push(sub_agent::definition());
            plan_tools
        }
    }
}

/// Returns tool definitions for sub-agents (read-only, no interactive or recursive tools).
pub fn get_sub_agent_tool_definitions() -> Vec<Value> {
    vec![
        read_file::definition(),
        glob::definition(),
        grep::definition(),
        list_dir::definition(),
        web_search::definition(),
        web_fetch::definition(),
    ]
}

/// Execute a tool by name with the given arguments.
pub async fn execute_tool(
    name: &str,
    args: Value,
    mode: Mode,
) -> ToolExecutionResult {
    // PLAN mode enforcement (sub_agent is intercepted in chat.rs, not executed here)
    if mode == Mode::Plan && !READ_ONLY_TOOLS.contains(name) && name != "sub_agent" && !name.starts_with("mcp__") {
        return ToolExecutionResult::text(format!(
            "Error: Tool \"{}\" is not available in PLAN mode. Switch to BUILDER mode (Tab) to use it.",
            name
        ));
    }

    let mut result = match name {
        "bash" => bash::execute(args).await,
        "read_file" => read_file::execute(args).await,
        "write_file" => write_file::execute(args).await,
        "edit_file" => edit_file::execute(args).await,
        "glob" => glob::execute(args).await,
        "grep" => grep::execute(args).await,
        "list_directory" => list_dir::execute(args).await,
        "web_search" => web_search::execute(args).await,
        "web_fetch" => web_fetch::execute(args).await,
        _ => ToolExecutionResult::text(format!("Error: Unknown tool \"{}\"", name)),
    };

    // Append system reminder to reinforce instructions across long conversations
    result.result.push_str(&system_reminder(mode, name));
    result
}
