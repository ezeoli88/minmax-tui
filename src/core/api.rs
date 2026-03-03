use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const BASE_URL: &str = "https://api.minimax.io/v1";
const QUOTA_ENDPOINTS: [&str; 4] = [
    "/api/openplatform/coding_plan/remains",
    "/coding_plan/remains",
    "https://www.minimax.io/v1/api/openplatform/coding_plan/remains",
    "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
];

// ── Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccumulatedToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct StreamResult {
    pub content: String,
    pub reasoning_details: Vec<String>,
    pub tool_calls: Vec<AccumulatedToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
}

/// Events emitted during streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ReasoningChunk(String),
    ContentChunk(String),
    ToolCallDelta(Vec<AccumulatedToolCall>),
    Done(Usage),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct QuotaInfo {
    pub used: u64,
    pub total: u64,
    pub remaining: u64,
    pub reset_minutes: u64,
}

// ── Client ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MiniMaxClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl MiniMaxClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.to_string(),
            base_url: BASE_URL.to_string(),
        }
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key)).unwrap(),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "X-Reasoning-Split",
            HeaderValue::from_static("true"),
        );
        headers
    }

    /// Fetch quota/plan remaining info.
    pub async fn fetch_quota(&self) -> Result<QuotaInfo> {
        let mut errors = Vec::new();

        for endpoint in QUOTA_ENDPOINTS {
            let url = if endpoint.starts_with("http") {
                endpoint.to_string()
            } else {
                format!("{}{}", self.base_url, endpoint)
            };

            let resp = match self
                .http
                .get(&url)
                .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    errors.push(format!("{} -> request error: {}", url, e));
                    continue;
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if let Ok(json) = serde_json::from_str::<Value>(&body) {
                    if let Some(mapped) = parse_minimax_error(&json) {
                        errors.push(format!("{} -> HTTP {} ({})", url, status, mapped));
                        continue;
                    }
                }
                let preview = if body.len() > 200 {
                    format!("{}...", &body[..200])
                } else {
                    body
                };
                errors.push(format!(
                    "{} -> HTTP {}{}",
                    url,
                    status,
                    if preview.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", preview)
                    }
                ));
                continue;
            }

            let data: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    errors.push(format!("{} -> invalid JSON: {}", url, e));
                    continue;
                }
            };

            match parse_quota_info(&data) {
                Ok(q) => return Ok(q),
                Err(e) => {
                    errors.push(format!("{} -> {}", url, e));
                }
            }
        }

        Err(anyhow!(
            "Quota API failed across all endpoints: {}",
            errors.join(" | ")
        ))
    }

    /// Stream a chat completion, sending events to the provided channel.
    /// Returns the final accumulated result.
    pub async fn stream_chat(
        &self,
        model: &str,
        messages: &[Value],
        tools: Option<&[Value]>,
        event_tx: Option<mpsc::UnboundedSender<StreamEvent>>,
        cancel: CancellationToken,
    ) -> Result<StreamResult> {
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
            "temperature": 0.3,
        });

        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::json!(tools);
                body["tool_choice"] = serde_json::json!("auto");
            }
        }

        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .http
            .post(&url)
            .headers(self.headers())
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if let Ok(json) = serde_json::from_str::<Value>(&text) {
                if let Some(mapped) = parse_minimax_error(&json) {
                    return Err(anyhow!("API error {}: {}", status, mapped));
                }
            }
            return Err(anyhow!("API error {}: {}", status, text));
        }

        let mut content = String::new();
        let mut reasoning_details: Vec<String> = Vec::new();
        let mut tool_calls_map: HashMap<usize, AccumulatedToolCall> = HashMap::new();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();
        let mut chunk_count: u64 = 0;

        let mut stream = response.bytes_stream();

        // SSE buffer for partial lines
        let mut line_buffer = String::new();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            line_buffer.push_str(&String::from_utf8_lossy(&bytes));

                            // Process complete SSE lines
                            while let Some(line_end) = line_buffer.find('\n') {
                                let line = line_buffer[..line_end].trim_end_matches('\r').to_string();
                                line_buffer = line_buffer[line_end + 1..].to_string();

                                if line.is_empty() || line.starts_with(':') {
                                    continue;
                                }

                                if let Some(data) = line.strip_prefix("data: ") {
                                    let data = data.trim();
                                    if data == "[DONE]" {
                                        continue;
                                    }

                                    if let Ok(chunk_json) = serde_json::from_str::<Value>(data) {
                                        chunk_count += 1;
                                        process_chunk(
                                            &chunk_json,
                                            &mut content,
                                            &mut reasoning_details,
                                            &mut tool_calls_map,
                                            &mut usage,
                                            &mut finish_reason,
                                            &event_tx,
                                        );
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            let msg = format!("Stream error: {}", e);
                            if let Some(tx) = &event_tx {
                                let _ = tx.send(StreamEvent::Error(msg.clone()));
                            }
                            return Err(anyhow!(msg));
                        }
                        None => break, // Stream ended
                    }
                }
            }
        }

        // Detect empty response
        if chunk_count == 0 && content.is_empty() && tool_calls_map.is_empty() {
            if let Some(tx) = &event_tx {
                let _ = tx.send(StreamEvent::Error(
                    "No response received from API (0 chunks)".to_string(),
                ));
            }
        }

        let tool_calls: Vec<AccumulatedToolCall> = {
            let mut entries: Vec<(usize, AccumulatedToolCall)> =
                tool_calls_map.into_iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            entries.into_iter().map(|(_, v)| v).collect()
        };

        if let Some(tx) = &event_tx {
            let _ = tx.send(StreamEvent::Done(usage.clone()));
        }

        Ok(StreamResult {
            content,
            reasoning_details,
            tool_calls,
            usage,
            finish_reason,
        })
    }

    /// Non-streaming completion for internal use (e.g. context summarization).
    pub async fn simple_completion(&self, model: &str, prompt: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "user", "content": prompt}
            ],
            "stream": false,
            "temperature": 0.3,
            "max_tokens": 2000,
        });

        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .http
            .post(&url)
            .headers(self.headers())
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if let Ok(json) = serde_json::from_str::<Value>(&text) {
                if let Some(mapped) = parse_minimax_error(&json) {
                    return Err(anyhow!("API error {}: {}", status, mapped));
                }
            }
            return Err(anyhow!("API error {}: {}", status, text));
        }

        let data: Value = response.json().await?;
        let content = data["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(content)
    }
}

fn parse_quota_info(data: &Value) -> Result<QuotaInfo> {
    let mut base_resp_error: Option<String> = None;
    if let Some(base_resp) = data.get("base_resp") {
        let status_code = read_u64_field(base_resp, &["status_code", "statusCode"]).unwrap_or(0);
        if status_code != 0 {
            let msg = base_resp
                .get("status_msg")
                .or_else(|| base_resp.get("statusMessage"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            base_resp_error = Some(format_minimax_error(status_code, msg));
        }
    }

    let remains_value = data
        .get("model_remains")
        .or_else(|| data.get("data").and_then(|d| d.get("model_remains")));

    let remains: Vec<&Value> = match remains_value {
        Some(Value::Array(arr)) => arr.iter().collect(),
        Some(Value::Object(_)) => remains_value.into_iter().collect(),
        _ => Vec::new(),
    };

    if remains.is_empty() {
        if let Some(err) = base_resp_error {
            return Err(anyhow!("{}", err));
        }
        return Err(anyhow!("No model_remains data in response"));
    }

    let entry = remains
        .iter()
        .max_by_key(|item| {
            read_u64_field(
                item,
                &[
                    "current_interval_total_count",
                    "current_interval_total",
                    "interval_total_count",
                    "total",
                ],
            )
            .unwrap_or(0)
        })
        .ok_or_else(|| anyhow!("model_remains is empty"))?;

    let total = read_u64_field(
        entry,
        &[
            "current_interval_total_count",
            "current_interval_total",
            "interval_total_count",
            "total",
        ],
    )
    .unwrap_or(0);

    let used = read_u64_field(
        entry,
        &[
            "current_interval_usage_count",
            "current_interval_used_count",
            "interval_usage_count",
            "used",
        ],
    )
    .unwrap_or_else(|| {
        let remaining_guess = read_u64_field(
            entry,
            &[
                "current_interval_remain_count",
                "current_interval_remaining_count",
                "remaining",
            ],
        )
        .unwrap_or(0);
        total.saturating_sub(remaining_guess)
    });

    let remaining = read_u64_field(
        entry,
        &[
            "current_interval_remain_count",
            "current_interval_remaining_count",
            "remaining",
        ],
    )
    .unwrap_or_else(|| total.saturating_sub(used));

    let remains_time_raw =
        read_u64_field(entry, &["remains_time", "remain_time", "reset_time"]).unwrap_or(0);

    if total == 0 && used == 0 && remaining == 0 {
        if let Some(err) = base_resp_error {
            return Err(anyhow!("{}", err));
        }
        return Err(anyhow!("Quota values missing in model_remains entry"));
    }

    Ok(QuotaInfo {
        used,
        total,
        remaining,
        reset_minutes: to_minutes(remains_time_raw),
    })
}

fn read_u64_field(data: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(value) = data.get(*key) {
            if let Some(parsed) = value_to_u64(value) {
                return Some(parsed);
            }
        }
    }
    None
}

fn value_to_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => {
            if let Some(v) = n.as_u64() {
                Some(v)
            } else {
                n.as_f64().map(|v| v.max(0.0) as u64)
            }
        }
        Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

fn to_minutes(raw: u64) -> u64 {
    if raw == 0 {
        0
    } else if raw > 100_000 {
        // Most responses use milliseconds; convert to rounded-up minutes.
        (raw + 59_999) / 60_000
    } else {
        // Some responses may provide seconds.
        (raw + 59) / 60
    }
}

fn parse_minimax_error(data: &Value) -> Option<String> {
    if let Some(base_resp) = data.get("base_resp").or_else(|| data.get("baseResp")) {
        if let Some(code) = read_u64_field(base_resp, &["status_code", "statusCode", "code"]) {
            if code != 0 {
                let msg = base_resp
                    .get("status_msg")
                    .or_else(|| base_resp.get("statusMessage"))
                    .or_else(|| base_resp.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Some(format_minimax_error(code, msg));
            }
        }
    }

    if let Some(error) = data.get("error") {
        if let Some(code) = read_u64_field(error, &["code", "status_code", "statusCode"]) {
            if code != 0 {
                let msg = error
                    .get("message")
                    .or_else(|| error.get("msg"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Some(format_minimax_error(code, msg));
            }
        }
    }

    if let Some(code) = read_u64_field(data, &["status_code", "statusCode", "error_code", "errorCode"]) {
        if code != 0 {
            let msg = data
                .get("status_msg")
                .or_else(|| data.get("statusMessage"))
                .or_else(|| data.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Some(format_minimax_error(code, msg));
        }
    }

    None
}

fn format_minimax_error(code: u64, msg: &str) -> String {
    match minimax_error_solution(code) {
        Some(solution) => format!("MiniMax error {}: {}. {}", code, msg, solution),
        None => format!("MiniMax error {}: {}", code, msg),
    }
}

fn minimax_error_solution(code: u64) -> Option<&'static str> {
    match code {
        1000 | 1001 | 1002 | 1024 | 1033 | 1039 => Some("Please retry your request later."),
        1004 | 2049 => Some("Check your API key and make sure it is correct and active."),
        1008 => Some("Check your account balance."),
        1026 | 1027 => Some("Change your input content."),
        1041 => Some("Connection limit reached; contact MiniMax if the issue persists."),
        1042 => Some("Check input for invisible or illegal characters."),
        1043 => Some("Check file_id and text_validation."),
        1044 => Some("Check clone prompt audio and prompt words."),
        2013 => Some("Check the request parameters."),
        20132 => Some("Check file_id/voice_id and contact MiniMax if the issue persists."),
        2037 => Some("Adjust the duration of your voice clone file."),
        2039 => Some("Use a non-duplicate voice_id."),
        2042 => Some("Check access permissions for this voice_id."),
        2045 => Some("Avoid sudden spikes/drops in request volume."),
        2048 => Some("Shorten prompt_audio to under 8 seconds."),
        2056 => Some("Usage limit exceeded; wait for the next 5-hour window."),
        _ => None,
    }
}

fn process_chunk(
    chunk: &Value,
    content: &mut String,
    reasoning_details: &mut Vec<String>,
    tool_calls_map: &mut HashMap<usize, AccumulatedToolCall>,
    usage: &mut Usage,
    finish_reason: &mut String,
    event_tx: &Option<mpsc::UnboundedSender<StreamEvent>>,
) {
    // Usage
    if let Some(u) = chunk.get("usage") {
        usage.prompt_tokens = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        usage.completion_tokens = u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        usage.total_tokens = u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    }

    // API-level error
    if let Some(err) = chunk.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown API error");
        if let Some(tx) = event_tx {
            let _ = tx.send(StreamEvent::Error(format!("API error: {}", msg)));
        }
        return;
    }

    let choice = match chunk.get("choices").and_then(|c| c.get(0)) {
        Some(c) => c,
        None => return,
    };

    if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        *finish_reason = fr.to_string();
    }

    let delta = match choice.get("delta") {
        Some(d) => d,
        None => return,
    };

    // Reasoning details
    if let Some(rd) = delta.get("reasoning_details").and_then(|v| v.as_array()) {
        for item in rd {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                reasoning_details.push(text.to_string());
                if let Some(tx) = event_tx {
                    let _ = tx.send(StreamEvent::ReasoningChunk(text.to_string()));
                }
            }
        }
    }
    if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        reasoning_details.push(rc.to_string());
        if let Some(tx) = event_tx {
            let _ = tx.send(StreamEvent::ReasoningChunk(rc.to_string()));
        }
    }

    // Content
    if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
        content.push_str(c);
        if let Some(tx) = event_tx {
            let _ = tx.send(StreamEvent::ContentChunk(c.to_string()));
        }
    }

    // Tool calls
    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let entry = tool_calls_map.entry(idx).or_insert_with(|| AccumulatedToolCall {
                id: String::new(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: String::new(),
                    arguments: String::new(),
                },
            });

            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                entry.id = id.to_string();
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                    entry.function.name = name.to_string();
                }
                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                    entry.function.arguments.push_str(args);
                }
            }
        }

        // Send accumulated state
        if let Some(tx) = event_tx {
            let mut entries: Vec<(usize, &AccumulatedToolCall)> =
                tool_calls_map.iter().map(|(k, v)| (*k, v)).collect();
            entries.sort_by_key(|(k, _)| *k);
            let accumulated: Vec<AccumulatedToolCall> =
                entries.into_iter().map(|(_, v)| v.clone()).collect();
            let _ = tx.send(StreamEvent::ToolCallDelta(accumulated));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_content_chunk() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "Hello"
                }
            }]
        });
        let mut content = String::new();
        let mut reasoning = Vec::new();
        let mut tool_calls = HashMap::new();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();

        process_chunk(
            &chunk,
            &mut content,
            &mut reasoning,
            &mut tool_calls,
            &mut usage,
            &mut finish_reason,
            &None,
        );

        assert_eq!(content, "Hello");
    }

    #[test]
    fn process_reasoning_chunk() {
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "reasoning_details": [{"text": "thinking..."}]
                }
            }]
        });
        let mut content = String::new();
        let mut reasoning = Vec::new();
        let mut tool_calls = HashMap::new();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();

        process_chunk(
            &chunk,
            &mut content,
            &mut reasoning,
            &mut tool_calls,
            &mut usage,
            &mut finish_reason,
            &None,
        );

        assert_eq!(reasoning, vec!["thinking..."]);
    }

    #[test]
    fn tool_call_accumulation() {
        let mut content = String::new();
        let mut reasoning = Vec::new();
        let mut tool_calls: HashMap<usize, AccumulatedToolCall> = HashMap::new();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();

        // First delta: tool call id + name
        let chunk1 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_123",
                        "function": { "name": "read_file", "arguments": "{\"pa" }
                    }]
                }
            }]
        });
        process_chunk(&chunk1, &mut content, &mut reasoning, &mut tool_calls, &mut usage, &mut finish_reason, &None);

        // Second delta: more arguments
        let chunk2 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "th\": \"main.rs\"}" }
                    }]
                }
            }]
        });
        process_chunk(&chunk2, &mut content, &mut reasoning, &mut tool_calls, &mut usage, &mut finish_reason, &None);

        assert_eq!(tool_calls.len(), 1);
        let tc = tool_calls.get(&0).unwrap();
        assert_eq!(tc.id, "call_123");
        assert_eq!(tc.function.name, "read_file");
        assert_eq!(tc.function.arguments, r#"{"path": "main.rs"}"#);
    }

    #[test]
    fn usage_extraction() {
        let chunk = serde_json::json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150
            },
            "choices": [{"delta": {}}]
        });
        let mut content = String::new();
        let mut reasoning = Vec::new();
        let mut tool_calls = HashMap::new();
        let mut usage = Usage::default();
        let mut finish_reason = String::new();

        process_chunk(&chunk, &mut content, &mut reasoning, &mut tool_calls, &mut usage, &mut finish_reason, &None);

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn parse_quota_info_standard_shape() {
        let payload = serde_json::json!({
            "base_resp": { "status_code": 0, "status_msg": "success" },
            "model_remains": [{
                "current_interval_total_count": 1500,
                "current_interval_usage_count": 15,
                "remains_time": 14940000
            }]
        });

        let quota = parse_quota_info(&payload).unwrap();
        assert_eq!(quota.total, 1500);
        assert_eq!(quota.used, 15);
        assert_eq!(quota.remaining, 1485);
        assert_eq!(quota.reset_minutes, 249);
    }

    #[test]
    fn parse_quota_info_nested_data_and_string_values() {
        let payload = serde_json::json!({
            "data": {
                "model_remains": [{
                    "current_interval_total_count": "1000",
                    "current_interval_usage_count": "10",
                    "remains_time": "14940"
                }]
            }
        });

        let quota = parse_quota_info(&payload).unwrap();
        assert_eq!(quota.total, 1000);
        assert_eq!(quota.used, 10);
        assert_eq!(quota.remaining, 990);
        assert_eq!(quota.reset_minutes, 249);
    }

    #[test]
    fn parse_quota_info_rejects_api_error_payload() {
        let payload = serde_json::json!({
            "base_resp": { "status_code": 1004, "status_msg": "invalid token" }
        });

        let err = parse_quota_info(&payload).unwrap_err().to_string();
        assert!(err.contains("MiniMax error 1004"));
    }

    #[test]
    fn parse_quota_info_accepts_model_remains_even_with_nonzero_status() {
        let payload = serde_json::json!({
            "base_resp": { "status_code": 1000, "status_msg": "unknown error" },
            "model_remains": [{
                "current_interval_total_count": 1200,
                "current_interval_usage_count": 12,
                "remains_time": 14940000
            }]
        });

        let quota = parse_quota_info(&payload).unwrap();
        assert_eq!(quota.total, 1200);
        assert_eq!(quota.used, 12);
        assert_eq!(quota.remaining, 1188);
        assert_eq!(quota.reset_minutes, 249);
    }

    #[test]
    fn to_minutes_handles_ms_and_seconds() {
        assert_eq!(to_minutes(14940000), 249);
        assert_eq!(to_minutes(14940), 249);
    }

    #[test]
    fn parse_minimax_error_from_base_resp() {
        let payload = serde_json::json!({
            "base_resp": { "status_code": 1004, "status_msg": "invalid token" }
        });
        let msg = parse_minimax_error(&payload).unwrap();
        assert!(msg.contains("MiniMax error 1004"));
        assert!(msg.contains("Check your API key"));
    }

    #[test]
    fn parse_minimax_error_from_error_object() {
        let payload = serde_json::json!({
            "error": { "code": 2056, "message": "usage limit exceeded" }
        });
        let msg = parse_minimax_error(&payload).unwrap();
        assert!(msg.contains("MiniMax error 2056"));
        assert!(msg.contains("5-hour window"));
    }
}
