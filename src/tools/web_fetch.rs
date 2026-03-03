use super::ToolExecutionResult;
use reqwest::header::{HeaderValue, ACCEPT, USER_AGENT};
use serde_json::Value;
use std::time::Duration;

pub fn definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "web_fetch",
            "description": "Fetch the content of a URL and return it as readable text. Use for reading documentation pages, GitHub issues/PRs, API references, or any web page the user references. Returns the page content converted to plain text (HTML tags stripped). Max 15KB output.",
            "parameters": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch (must be a valid http/https URL)"
                    }
                },
                "required": ["url"]
            }
        }
    })
}

pub async fn execute(args: Value) -> ToolExecutionResult {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if url.is_empty() {
        return ToolExecutionResult::text("Error: No URL provided".to_string());
    }

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return ToolExecutionResult::text(
            "Error: URL must start with http:// or https://".to_string(),
        );
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let response = match client
        .get(url)
        .header(USER_AGENT, HeaderValue::from_static(
            "Mozilla/5.0 (compatible; MinMaxCode/1.0; +https://github.com/minmax-code)"
        ))
        .header(ACCEPT, HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.7"
        ))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.is_timeout() {
                return ToolExecutionResult::text(
                    "Error: Request timed out after 15 seconds.".to_string(),
                );
            }
            if e.is_connect() {
                return ToolExecutionResult::text(
                    "Error: Could not connect. Check internet connection.".to_string(),
                );
            }
            return ToolExecutionResult::text(format!("Error fetching URL: {}", e));
        }
    };

    let status = response.status();
    if !status.is_success() {
        return ToolExecutionResult::text(format!(
            "Error: HTTP {} for {}",
            status, url
        ));
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let body = match response.text().await {
        Ok(t) => t,
        Err(e) => return ToolExecutionResult::text(format!("Error reading response body: {}", e)),
    };

    let text = if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        html_to_text(&body)
    } else {
        body
    };

    // Truncate to 15KB
    let max_len = 15_000;
    let output = if text.len() > max_len {
        let safe_end = text
            .char_indices()
            .take_while(|(idx, _)| *idx < max_len)
            .last()
            .map(|(idx, c)| idx + c.len_utf8())
            .unwrap_or(max_len);
        format!(
            "{}\n\n[Content truncated at ~15KB, originally {} bytes]",
            &text[..safe_end],
            text.len()
        )
    } else {
        text
    };

    if output.trim().is_empty() {
        return ToolExecutionResult::text(format!(
            "Page fetched but no readable text content found at {}",
            url
        ));
    }

    ToolExecutionResult::text(format!("Content from {}:\n\n{}", url, output))
}

/// Minimal HTML-to-text conversion: strips tags, decodes common entities,
/// collapses whitespace, and preserves basic structure from block elements.
fn html_to_text(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut tag_buf = String::new();

    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let tag_lower = tag_buf.to_lowercase();
                let tag_name = tag_lower
                    .split_whitespace()
                    .next()
                    .unwrap_or("");

                if tag_name == "script" {
                    in_script = true;
                } else if tag_name == "/script" {
                    in_script = false;
                } else if tag_name == "style" {
                    in_style = true;
                } else if tag_name == "/style" {
                    in_style = false;
                }

                // Add newlines for block-level elements
                if matches!(
                    tag_name,
                    "br" | "br/" | "p" | "/p" | "div" | "/div" | "h1" | "/h1"
                        | "h2" | "/h2" | "h3" | "/h3" | "h4" | "/h4"
                        | "h5" | "/h5" | "h6" | "/h6" | "li" | "tr" | "/tr"
                        | "blockquote" | "/blockquote" | "pre" | "/pre"
                        | "hr" | "hr/"
                ) {
                    result.push('\n');
                }
            }
            _ if in_tag => {
                tag_buf.push(ch);
            }
            _ if in_script || in_style => {}
            _ => {
                result.push(ch);
            }
        }
    }

    // Decode common HTML entities
    let result = result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
        .replace("&#x2F;", "/")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…");

    // Collapse runs of whitespace and blank lines
    let mut collapsed = String::with_capacity(result.len());
    let mut prev_newline_count = 0;
    let mut prev_was_space = false;

    for ch in result.chars() {
        if ch == '\n' {
            prev_newline_count += 1;
            prev_was_space = false;
            if prev_newline_count <= 2 {
                collapsed.push('\n');
            }
        } else if ch.is_whitespace() {
            prev_newline_count = 0;
            if !prev_was_space {
                collapsed.push(' ');
                prev_was_space = true;
            }
        } else {
            prev_newline_count = 0;
            prev_was_space = false;
            collapsed.push(ch);
        }
    }

    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags() {
        let html = "<p>Hello <strong>world</strong></p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello world"));
        assert!(!text.contains("<p>"));
        assert!(!text.contains("<strong>"));
    }

    #[test]
    fn html_to_text_handles_entities() {
        let html = "foo &amp; bar &lt;baz&gt;";
        let text = html_to_text(html);
        assert_eq!(text, "foo & bar <baz>");
    }

    #[test]
    fn html_to_text_strips_scripts() {
        let html = "<p>Before</p><script>alert('xss')</script><p>After</p>";
        let text = html_to_text(html);
        assert!(text.contains("Before"));
        assert!(text.contains("After"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn html_to_text_strips_styles() {
        let html = "<style>.foo { color: red; }</style><p>Content</p>";
        let text = html_to_text(html);
        assert!(text.contains("Content"));
        assert!(!text.contains("color"));
    }
}
