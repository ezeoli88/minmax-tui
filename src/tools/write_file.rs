use super::{ToolExecutionResult, ToolResultMeta};
use serde_json::Value;
use std::path::Path;
use tokio::fs;

pub fn definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Create or overwrite a file with the given content. Creates parent directories automatically. WARNING: Completely replaces existing content. For partial edits use edit_file instead.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }
        }
    })
}

pub async fn execute(args: Value) -> ToolExecutionResult {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if path.is_empty() {
        return ToolExecutionResult::text("Error: No path provided".to_string());
    }

    let is_new = !Path::new(path).exists();

    // Create parent directories
    if let Some(parent) = Path::new(path).parent() {
        if !parent.exists() {
            if let Err(e) = fs::create_dir_all(parent).await {
                return ToolExecutionResult::text(format!("Error creating directories: {}", e));
            }
        }
    }

    match fs::write(path, content).await {
        Ok(_) => {
            let mut msg = format!("File written successfully: {}", path);

            // Run syntax check on the written file
            if let Some(diag) = super::lint::check_syntax(path).await {
                msg.push_str(&diag);
            }

            ToolExecutionResult::with_meta(
                msg,
                ToolResultMeta::WriteFile {
                    path: path.to_string(),
                    content: content.to_string(),
                    is_new,
                },
            )
        }
        Err(e) => ToolExecutionResult::text(format!("Error writing file: {}", e)),
    }
}
