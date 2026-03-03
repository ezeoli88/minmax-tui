use super::{ToolExecutionResult, ToolResultMeta};
use serde_json::Value;
use std::path::Path;
use tokio::fs;

pub fn definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "edit_file",
            "description": "Replace an exact string in a file. old_str must match exactly once (including whitespace/indentation). If old_str appears 0 or >1 times, the edit fails — add more surrounding context to make it unique. Preferred over write_file for modifying existing files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_str": {
                        "type": "string",
                        "description": "The exact string to find and replace. Must be unique in the file."
                    },
                    "new_str": {
                        "type": "string",
                        "description": "The replacement string"
                    }
                },
                "required": ["path", "old_str", "new_str"]
            }
        }
    })
}

pub async fn execute(args: Value) -> ToolExecutionResult {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let old_str = args.get("old_str").and_then(|v| v.as_str()).unwrap_or("");
    let new_str = args.get("new_str").and_then(|v| v.as_str()).unwrap_or("");

    if path.is_empty() {
        return ToolExecutionResult::text("Error: No path provided".to_string());
    }

    if !Path::new(path).exists() {
        return ToolExecutionResult::text(format!("Error: File not found: {}", path));
    }

    let content = match fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) => return ToolExecutionResult::text(format!("Error reading file: {}", e)),
    };

    let occurrences = content.matches(old_str).count();
    if occurrences == 0 {
        return ToolExecutionResult::text(format!("Error: old_str not found in {}", path));
    }
    if occurrences > 1 {
        return ToolExecutionResult::text(format!(
            "Error: old_str found {} times in {}. It must be unique. Add more context to make it unique.",
            occurrences, path
        ));
    }

    let new_content = content.replacen(old_str, new_str, 1);
    match fs::write(path, &new_content).await {
        Ok(_) => {
            let mut msg = format!("File edited successfully: {}", path);

            // Run syntax check on the edited file
            if let Some(diag) = super::lint::check_syntax(path).await {
                msg.push_str(&diag);
            }

            ToolExecutionResult::with_meta(
                msg,
                ToolResultMeta::EditFile {
                    path: path.to_string(),
                    old_str: old_str.to_string(),
                    new_str: new_str.to_string(),
                },
            )
        }
        Err(e) => ToolExecutionResult::text(format!("Error writing file: {}", e)),
    }
}
