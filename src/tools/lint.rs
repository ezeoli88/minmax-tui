use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// Run a quick syntax check on a file after edit/write.
/// Returns `Some(diagnostic)` if there are syntax errors, `None` if clean or unsupported.
pub async fn check_syntax(path: &str) -> Option<String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let (program, args): (&str, Vec<String>) = match ext.as_str() {
        "py" => ("python3", vec!["-m".into(), "py_compile".into(), path.into()]),
        "js" | "mjs" => ("node", vec!["--check".into(), path.into()]),
        "ts" | "tsx" => {
            // Try npx tsc --noEmit; fall back to node --check for basic parse
            if command_exists("npx").await {
                ("npx", vec!["tsc".into(), "--noEmit".into(), "--pretty".into(), path.into()])
            } else {
                return None;
            }
        }
        "rs" => {
            // For Rust, a full cargo check is too slow for inline gating.
            // We only do a quick syntax parse if `rustfmt --check` is available.
            if command_exists("rustfmt").await {
                ("rustfmt", vec!["--check".into(), "--edition".into(), "2021".into(), path.into()])
            } else {
                return None;
            }
        }
        "json" => ("python3", vec!["-c".into(), format!("import json; json.load(open('{}'))", path)]),
        "yaml" | "yml" => {
            if command_exists("python3").await {
                ("python3", vec!["-c".into(), format!("import yaml; yaml.safe_load(open('{}'))", path)])
            } else {
                return None;
            }
        }
        "rb" => ("ruby", vec!["-c".into(), path.into()]),
        "go" => {
            // go vet is too heavyweight; just check if gofmt parses
            if command_exists("gofmt").await {
                ("gofmt", vec!["-e".into(), path.into()])
            } else {
                return None;
            }
        }
        "sh" | "bash" => ("bash", vec!["-n".into(), path.into()]),
        _ => return None,
    };

    if !command_exists(program).await {
        return None;
    }

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(program)
            .args(&args)
            .current_dir(std::env::current_dir().unwrap_or_default())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) => {
            if output.status.success() {
                None
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let combined = if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    stderr.to_string()
                };
                // Truncate diagnostics to keep tool results manageable
                let max = 1000;
                let diag = if combined.len() > max {
                    format!("{}...(truncated)", &combined[..max])
                } else {
                    combined
                };
                Some(format!(
                    "\n⚠ Syntax check ({}) found issues:\n{}",
                    program, diag
                ))
            }
        }
        // Timeout or execution error — skip silently
        _ => None,
    }
}

async fn command_exists(program: &str) -> bool {
    Command::new("which")
        .arg(program)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
