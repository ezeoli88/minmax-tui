use ratatui::prelude::*;
use ratatui::text::{Line as TuiLine, Span};

use crate::config::themes::Theme;
use crate::core::api::AccumulatedToolCall;
use crate::tui::app::{DisplayMessage, ToolStatus};

/// Render a tool call reference line (shown in assistant messages).
/// Format: → tool_name(args_preview)
pub fn render_tool_call_line<'a>(tc: &AccumulatedToolCall, theme: &Theme) -> TuiLine<'a> {
    let warning = Color::Rgb(theme.warning.r, theme.warning.g, theme.warning.b);
    let dim = Color::Rgb(theme.dim_text.r, theme.dim_text.g, theme.dim_text.b);

    let args_preview = abbreviate_args(&tc.function.arguments, 60);

    TuiLine::from(vec![
        Span::raw("  "),
        Span::styled("→ ", Style::default().fg(warning)),
        Span::styled(tc.function.name.clone(), Style::default().fg(warning).bold()),
        Span::styled(
            format!("({})", args_preview),
            Style::default().fg(dim),
        ),
    ])
}

/// Render tool result message lines.
/// Format:
///   ⚡ tool_name ✓/✗/...
///     preview lines
pub fn render_tool_result_lines<'a>(msg: &DisplayMessage, theme: &Theme, width: u16) -> Vec<TuiLine<'a>> {
    let warning = Color::Rgb(theme.warning.r, theme.warning.g, theme.warning.b);
    let success = Color::Rgb(theme.success.r, theme.success.g, theme.success.b);
    let error = Color::Rgb(theme.error.r, theme.error.g, theme.error.b);
    let dim = Color::Rgb(theme.dim_text.r, theme.dim_text.g, theme.dim_text.b);

    let mut lines = Vec::new();

    // Header line: ⚡ tool_name [status]
    let tool_name = msg.tool_name.as_deref().unwrap_or("tool");
    let (status_icon, status_color) = match &msg.tool_status {
        Some(ToolStatus::Running) => ("…", warning),
        Some(ToolStatus::Done) => ("✓", success),
        Some(ToolStatus::Error) => ("✗", error),
        None => ("", dim),
    };

    lines.push(TuiLine::from(vec![
        Span::raw("  "),
        Span::styled("⚡ ", Style::default().fg(warning)),
        Span::styled(tool_name.to_string(), Style::default().fg(warning).bold()),
        Span::raw(" "),
        Span::styled(status_icon.to_string(), Style::default().fg(status_color)),
    ]));

    // Sub-agent tool progress (compact inline display)
    if tool_name == "sub_agent" && !msg.sub_tools.is_empty() {
        let mut spans = vec![Span::raw("    ")];
        for (name, status) in &msg.sub_tools {
            let (icon, color) = match status {
                ToolStatus::Running => ("…", warning),
                ToolStatus::Done => ("✓", success),
                ToolStatus::Error => ("✗", error),
            };
            spans.push(Span::styled(format!("{} ", name), Style::default().fg(dim)));
            spans.push(Span::styled(format!("{}  ", icon), Style::default().fg(color)));
        }
        lines.push(TuiLine::from(spans));
    }

    // Content preview (truncated)
    if !msg.content.is_empty() {
        let max_preview_lines = 8;
        let content_width = (width.saturating_sub(6)) as usize;
        let preview_lines: Vec<&str> = msg.content.lines().take(max_preview_lines).collect();

        for line in &preview_lines {
            // Strip ANSI escape codes and replace tabs with spaces
            let clean = strip_ansi_and_tabs(line);
            let truncated = if clean.chars().count() > content_width {
                truncate_chars(&clean, content_width.saturating_sub(1))
            } else {
                clean
            };
            lines.push(TuiLine::from(vec![
                Span::raw("    "),
                Span::styled(truncated, Style::default().fg(dim)),
            ]));
        }

        let total_lines = msg.content.lines().count();
        if total_lines > max_preview_lines {
            lines.push(TuiLine::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("... ({} more lines)", total_lines - max_preview_lines),
                    Style::default().fg(dim).italic(),
                ),
            ]));
        }
    }

    lines
}

/// Strip ANSI escape sequences and replace tabs with spaces.
fn strip_ansi_and_tabs(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip ANSI escape: ESC [ ... final_byte
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
        } else if ch == '\t' {
            result.push_str("    ");
        } else if ch == '\r' {
            // skip carriage returns
        } else {
            result.push(ch);
        }
    }
    result
}

/// Truncate a string to at most `max_chars` characters, appending `…` if truncated.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut end = 0;
    for (i, (idx, _)) in s.char_indices().enumerate() {
        if i >= max_chars {
            return format!("{}…", &s[..end]);
        }
        end = idx + s[idx..].chars().next().map_or(0, |c| c.len_utf8());
    }
    s.to_string()
}

/// Abbreviate JSON arguments for display.
fn abbreviate_args(args_json: &str, max_len: usize) -> String {
    // Try to parse and show key-value pairs
    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(map) = obj.as_object() {
            let pairs: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let v_str = match v {
                        serde_json::Value::String(s) => {
                            if s.chars().count() > 30 {
                                format!("\"{}\"", truncate_chars(s, 27))
                            } else {
                                format!("\"{}\"", s)
                            }
                        }
                        _ => {
                            let s = v.to_string();
                            if s.chars().count() > 30 {
                                truncate_chars(&s, 27)
                            } else {
                                s
                            }
                        }
                    };
                    format!("{}={}", k, v_str)
                })
                .collect();
            let result = pairs.join(", ");
            if result.chars().count() > max_len {
                return truncate_chars(&result, max_len.saturating_sub(1));
            }
            return result;
        }
    }

    if args_json.chars().count() <= max_len {
        return args_json.to_string();
    }
    truncate_chars(args_json, max_len.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviate_short_args() {
        let args = r#"{"path":"main.rs"}"#;
        let result = abbreviate_args(args, 60);
        assert_eq!(result, r#"path="main.rs""#);
    }

    #[test]
    fn abbreviate_long_args() {
        let long_val = "a".repeat(100);
        let args = format!(r#"{{"content":"{}"}}"#, long_val);
        let result = abbreviate_args(&args, 60);
        // The value should be truncated (either individual value truncated to 30 chars,
        // or the whole result truncated to max_len)
        assert!(result.len() <= 60 || result.contains('…'));
    }
}
