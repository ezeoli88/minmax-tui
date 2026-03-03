use anyhow::Result;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::core::api::{AccumulatedToolCall, MiniMaxClient, StreamEvent};
use crate::core::mcp::McpManager;
use crate::core::parser::{coerce_arg, parse_model_output};
use crate::core::session::SessionStore;
use crate::core::Mode;
use crate::tools;

// ── Agent Question types ─────────────────────────────────────────────────

/// A question the agent wants to ask the user interactively.
#[derive(Debug, Clone)]
pub struct AgentQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    pub allow_custom: bool,
}

/// A batch of questions the agent wants to ask at once.
#[derive(Debug, Clone)]
pub struct AgentQuestionBatch {
    pub questions: Vec<AgentQuestion>,
}

/// Wrapper around the response channel that implements Debug and Clone.
#[derive(Clone)]
pub struct ResponseChannel(pub Arc<Mutex<Option<oneshot::Sender<String>>>>);

impl std::fmt::Debug for ResponseChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponseChannel(..)")
    }
}

// ── Todo types ───────────────────────────────────────────────────────────

/// Status of a todo item.
#[derive(Debug, Clone, PartialEq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// A single todo item in the task list.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

// ── Chat Events (for UI consumption) ────────────────────────────────────

/// Events emitted by the ChatEngine for UI updates.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// A new assistant streaming message started.
    StreamStart,
    /// Reasoning chunk received.
    ReasoningChunk(String),
    /// Content chunk received.
    ContentChunk(String),
    /// Tool calls accumulated so far.
    ToolCallsUpdate(Vec<AccumulatedToolCall>),
    /// Streaming finished, final message ready.
    StreamEnd(FinalMessage),
    /// A tool started executing.
    ToolExecutionStart { id: String, name: String },
    /// A tool finished executing.
    ToolExecutionDone {
        id: String,
        name: String,
        result: String,
    },
    /// Error during streaming or tool execution.
    Error(String),
    /// Updated token usage (prompt, completion, total).
    TokenUsage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },
    /// The agent needs to ask the user one or more questions interactively.
    AskUser {
        batch: AgentQuestionBatch,
        response_tx: ResponseChannel,
    },
    /// The agent updated the todo/task list.
    TodoUpdate(Vec<TodoItem>),
    /// The context was compressed to fit within the token window.
    ContextCompressed {
        original_tokens: usize,
        compressed_tokens: usize,
    },
    /// Progress from a running sub-agent (tool start/done events forwarded).
    SubAgentProgress {
        id: String,
        tool_name: String,
        done: bool,
        error: bool,
    },
}

/// The final assistant message after streaming completes.
#[derive(Debug, Clone)]
pub struct FinalMessage {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<AccumulatedToolCall>,
}

// ── Chat Message (for API history) ──────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Option<Vec<AccumulatedToolCall>>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

// ── Chat Engine ─────────────────────────────────────────────────────────

pub struct ChatEngine {
    client: MiniMaxClient,
    model: String,
    mode: Mode,
    history: Vec<Value>,
    session_id: Option<String>,
    session_store: Option<Arc<SessionStore>>,
    total_tokens: u64,
    pub accumulated_completion_tokens: u64,
    cancel_token: CancellationToken,
    mcp_manager: Option<Arc<tokio::sync::Mutex<McpManager>>>,
    max_turns: Option<usize>,
    is_sub_agent: bool,
}

impl ChatEngine {
    pub fn new(client: MiniMaxClient, model: &str, mode: Mode) -> Self {
        Self {
            client,
            model: model.to_string(),
            mode,
            history: Vec::new(),
            session_id: None,
            session_store: None,
            total_tokens: 0,
            accumulated_completion_tokens: 0,
            cancel_token: CancellationToken::new(),
            mcp_manager: None,
            max_turns: None,
            is_sub_agent: false,
        }
    }

    pub fn set_mcp_manager(&mut self, manager: Arc<tokio::sync::Mutex<McpManager>>) {
        self.mcp_manager = Some(manager);
    }

    pub fn set_session(&mut self, session_id: String, store: Arc<SessionStore>) {
        self.session_id = Some(session_id);
        self.session_store = Some(store);
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    pub fn set_max_turns(&mut self, limit: usize) {
        self.max_turns = Some(limit);
    }

    pub fn set_sub_agent(&mut self, val: bool) {
        self.is_sub_agent = val;
    }

    /// Extract the last assistant message content from history.
    pub fn last_assistant_content(&self) -> Option<String> {
        self.history
            .iter()
            .rev()
            .find(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .and_then(|msg| msg.get("content").and_then(|c| c.as_str()))
            .map(|s| s.to_string())
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.total_tokens = 0;
        self.accumulated_completion_tokens = 0;
        self.cancel_token = CancellationToken::new();
    }

    /// Load history from stored messages.
    pub fn load_history(&mut self, messages: Vec<Value>) {
        self.history = messages;
    }

    fn get_system_prompt(&self) -> String {
        let cwd = std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Sub-agents get a focused, minimal system prompt
        if self.is_sub_agent {
            return format!(
                "You are a research sub-agent with read-only codebase access.\n\
                Working directory: {}\n\n\
                Available tools: read_file, glob, grep, list_directory, web_search, web_fetch.\n\
                You CANNOT write, edit, run commands, ask questions, or spawn sub-agents.\n\n\
                Investigate thoroughly, then provide a complete, structured answer.\n\
                Your final message must contain the full answer — it will be returned to the parent agent.",
                cwd
            );
        }

        let base = match self.mode {
            Mode::Plan => format!(
                "You are a coding assistant in a terminal (READ-ONLY mode).\n\
                Working directory: {}\n\n\
                Available tools: read_file, glob, grep, list_directory, web_search, web_fetch (read-only), ask_user, todo_write.\n\
                You CANNOT write, edit, or run commands in this mode.\n\
                Focus on: analysis, planning, explaining code, suggesting implementation strategies.\n\
                IMPORTANT: Never tell the user to manually copy, paste, or create files themselves. \
                When proposing changes, explain what needs to be done and assure the user that \
                you will implement all changes automatically when they switch to BUILDER mode (Tab). \
                Your plans should describe the changes clearly but always frame execution as something you (the agent) will do.\n\
                Use ask_user to ask the user clarifying questions when you need more information to proceed. \
                IMPORTANT: If you have multiple questions, batch them ALL into a single ask_user call using the \"questions\" array parameter. \
                Never ask questions one at a time — group every question you have into one ask_user call.\n\
                Use todo_write to create a task list when you have a plan ready, tracking progress on multi-step work.",
                cwd
            ),
            Mode::Builder => format!(
                "You are a coding assistant in a terminal.\n\
                Working directory: {}\n\n\
                TOOL USAGE:\n\
                - Read before editing: always use read_file before edit_file to see current content\n\
                - Use edit_file for modifications to existing files, write_file only for new files\n\
                - Use glob/grep to find files before reading them\n\
                - Use bash for git, npm, and other CLI operations\n\
                - Use web_search for current information; use web_fetch to read the content of a specific URL\n\
                - Use ask_user when you need user clarification, confirmation, or to let the user choose between alternatives. \
                IMPORTANT: If you have multiple questions, batch them ALL into a single ask_user call using the \"questions\" array. Never ask one at a time.\n\
                - Use todo_write to create and update a task list when working on multi-step tasks\n\
                - Execute one logical step at a time, verify results, then proceed\n\n\
                Be concise. Show relevant code, skip obvious explanations.",
                cwd
            ),
        };

        // Load agent.md if present
        let agent_path = std::path::Path::new(&cwd).join("agent.md");
        if agent_path.exists() {
            if let Ok(agent_content) = std::fs::read_to_string(&agent_path) {
                return format!("{}\n\n--- agent.md ---\n{}", base, agent_content);
            }
        }

        base
    }

    fn build_full_history(&self) -> Vec<Value> {
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": self.get_system_prompt()
        })];

        let history_len = self.history.len();
        // Keep the last ~3 turns intact (each turn ≈ 2 messages: user+assistant or tool)
        let recent_threshold = history_len.saturating_sub(6);

        for (i, msg) in self.history.iter().enumerate() {
            let mut msg = msg.clone();
            let is_old = i < recent_threshold;

            if is_old {
                // Strip reasoning_details from old assistant messages — the model
                // doesn't need its own prior reasoning to continue the conversation.
                if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
                    if let Some(obj) = msg.as_object_mut() {
                        obj.remove("reasoning_details");
                    }
                }

                // Truncate long tool results to save context space.
                if msg.get("role").and_then(|r| r.as_str()) == Some("tool") {
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        if content.len() > 2000 {
                            let safe_end = content
                                .char_indices()
                                .nth(500)
                                .map(|(idx, _)| idx)
                                .unwrap_or(content.len().min(500));
                            let truncated = format!(
                                "{}...\n[truncated, originally {} chars]",
                                &content[..safe_end],
                                content.len()
                            );
                            msg["content"] = serde_json::json!(truncated);
                        }
                    }
                }
            }

            messages.push(msg);
        }

        messages
    }

    /// Estimate the total number of tokens in the history using a chars/4 heuristic.
    fn estimate_history_tokens(&self) -> usize {
        self.history
            .iter()
            .map(|msg| {
                let content_len = msg
                    .get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                let reasoning_len = msg
                    .get("reasoning_details")
                    .and_then(|r| serde_json::to_string(r).ok())
                    .map(|s| s.len())
                    .unwrap_or(0);
                let tool_args_len = msg
                    .get("tool_calls")
                    .and_then(|t| serde_json::to_string(t).ok())
                    .map(|s| s.len())
                    .unwrap_or(0);
                (content_len + reasoning_len + tool_args_len) / 4
            })
            .sum()
    }

    /// Compress old history when estimated tokens exceed the threshold.
    ///
    /// Uses a two-phase strategy:
    /// 1. **Granular truncation** — collapse individual old tool results to
    ///    one-line summaries preserving file paths and key metadata, strip old
    ///    reasoning, and trim verbose assistant messages.
    /// 2. **Summarization** — if granular truncation isn't enough (still over
    ///    the threshold), summarize the oldest messages via an API call.
    ///
    /// Recent messages are always preserved intact.
    async fn compress_history(
        &mut self,
        event_tx: &mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {
        const COMPRESSION_THRESHOLD: usize = 100_000;
        const MESSAGES_TO_KEEP: usize = 10;

        let estimated = self.estimate_history_tokens();
        if estimated < COMPRESSION_THRESHOLD {
            return Ok(());
        }

        let keep_count = MESSAGES_TO_KEEP.min(self.history.len());
        let split_point = self.history.len().saturating_sub(keep_count);
        if split_point == 0 {
            return Ok(());
        }

        // ── Phase 1: Granular truncation of old messages ──────────────
        for msg in self.history[..split_point].iter_mut() {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

            match role {
                "tool" => {
                    // Collapse tool results to a compact summary preserving paths
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        if content.len() > 300 {
                            let tool_name = msg
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool");
                            let summary = collapse_tool_result(tool_name, content);
                            msg["content"] = serde_json::json!(summary);
                        }
                    }
                }
                "assistant" => {
                    // Strip reasoning details from old assistant messages
                    if let Some(obj) = msg.as_object_mut() {
                        obj.remove("reasoning_details");
                    }
                    // Truncate verbose assistant content (keep first 300 chars)
                    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
                        if content.len() > 500 {
                            let safe_end = content
                                .char_indices()
                                .nth(300)
                                .map(|(idx, _)| idx)
                                .unwrap_or(300);
                            let truncated = format!(
                                "{}... [truncated from {} chars]",
                                &content[..safe_end],
                                content.len()
                            );
                            msg["content"] = serde_json::json!(truncated);
                        }
                    }
                }
                _ => {}
            }
        }

        // Re-estimate after granular truncation
        let after_granular = self.estimate_history_tokens();
        if after_granular < COMPRESSION_THRESHOLD {
            let _ = event_tx.send(ChatEvent::ContextCompressed {
                original_tokens: estimated,
                compressed_tokens: after_granular,
            });
            return Ok(());
        }

        // ── Phase 2: Summarize oldest messages via API call ───────────
        let old_messages: Vec<String> = self.history[..split_point]
            .iter()
            .map(|m| {
                let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                let content: String = m
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(500)
                    .collect();
                format!("[{}]: {}", role, content)
            })
            .collect();

        let summary_prompt = format!(
            "Summarize this coding assistant conversation history concisely. Preserve:\n\
             - Key decisions and context\n\
             - ALL file paths that were read, modified, or created\n\
             - Current task state and progress\n\
             - Important code patterns or bugs found\n\
             - Tool names and their outcomes\n\n\
             Conversation:\n{}",
            old_messages.join("\n")
        );

        let summary = self
            .client
            .simple_completion(&self.model, &summary_prompt)
            .await?;

        // Replace old messages with the summary
        let recent = self.history.split_off(split_point);
        self.history.clear();
        self.history.push(serde_json::json!({
            "role": "user",
            "content": format!("[Previous conversation summary]\n{}", summary)
        }));
        self.history.push(serde_json::json!({
            "role": "assistant",
            "content": "Understood. I have the context from our previous conversation. Let me continue."
        }));
        self.history.extend(recent);

        let compressed = self.estimate_history_tokens();
        let _ = event_tx.send(ChatEvent::ContextCompressed {
            original_tokens: estimated,
            compressed_tokens: compressed,
        });

        Ok(())
    }

    /// Send a user message and run the agentic loop.
    /// Emits ChatEvents to the provided sender for UI updates.
    /// Set an external cancel token (from the UI) so Esc can interrupt the agentic loop.
    pub fn set_cancel_token(&mut self, token: CancellationToken) {
        self.cancel_token = token;
    }

    pub fn send_message<'a>(
        &'a mut self,
        user_input: &'a str,
        file_context: Option<&'a str>,
        event_tx: mpsc::UnboundedSender<ChatEvent>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.send_message_inner(user_input, file_context, event_tx))
    }

    async fn send_message_inner(
        &mut self,
        user_input: &str,
        file_context: Option<&str>,
        event_tx: mpsc::UnboundedSender<ChatEvent>,
    ) -> Result<()> {

        // Build API content with file context if present
        let api_content = match file_context {
            Some(ctx) => format!("{}\n\nUser request: {}", ctx, user_input),
            None => user_input.to_string(),
        };

        // Add user message to history
        self.history.push(serde_json::json!({
            "role": "user",
            "content": api_content
        }));

        // Persist user message
        self.persist_message("user", user_input, None, None, None);

        // Agentic loop
        let mut turn_count: usize = 0;
        loop {
            if self.cancel_token.is_cancelled() {
                break;
            }
            if let Some(max) = self.max_turns {
                if turn_count >= max {
                    break;
                }
            }
            turn_count += 1;

            let _ = event_tx.send(ChatEvent::StreamStart);

            // Compress history if approaching context window limit
            if let Err(e) = self.compress_history(&event_tx).await {
                let _ = event_tx.send(ChatEvent::Error(format!(
                    "Context compression failed (non-fatal): {}",
                    e
                )));
            }

            let mut tool_defs = if self.is_sub_agent {
                tools::get_sub_agent_tool_definitions()
            } else {
                tools::get_tool_definitions(self.mode)
            };
            // Append MCP tool definitions if available (not for sub-agents)
            if !self.is_sub_agent {
                if let Some(mcp) = &self.mcp_manager {
                    if let Ok(manager) = mcp.try_lock() {
                        tool_defs.extend(manager.get_tool_definitions());
                    }
                }
            }
            let full_history = self.build_full_history();

            // Create a channel for stream events
            let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<StreamEvent>();

            // Forward stream events to chat events
            let event_tx_clone = event_tx.clone();
            let forward_handle = tokio::spawn(async move {
                while let Some(evt) = stream_rx.recv().await {
                    let chat_evt = match evt {
                        StreamEvent::ReasoningChunk(c) => ChatEvent::ReasoningChunk(c),
                        StreamEvent::ContentChunk(c) => ChatEvent::ContentChunk(c),
                        StreamEvent::ToolCallDelta(tcs) => ChatEvent::ToolCallsUpdate(tcs),
                        StreamEvent::Done(usage) => ChatEvent::TokenUsage {
                            prompt_tokens: usage.prompt_tokens,
                            completion_tokens: usage.completion_tokens,
                            total_tokens: usage.total_tokens,
                        },
                        StreamEvent::Error(e) => ChatEvent::Error(e),
                    };
                    let _ = event_tx_clone.send(chat_evt);
                }
            });

            let result = self
                .client
                .stream_chat(
                    &self.model,
                    &full_history,
                    Some(&tool_defs),
                    Some(stream_tx),
                    self.cancel_token.clone(),
                )
                .await;

            // Wait for forwarding to finish
            let _ = forward_handle.await;

            let result = match result {
                Ok(r) => r,
                Err(e) => {
                    let _ = event_tx.send(ChatEvent::Error(format!("Stream error: {}", e)));
                    break;
                }
            };

            // prompt_tokens includes the full context window each request (system prompt +
            // history + tools), so we use the latest value instead of accumulating.
            // completion_tokens are new tokens generated per request, so we accumulate those.
            self.accumulated_completion_tokens += result.usage.completion_tokens;
            self.total_tokens = result.usage.prompt_tokens + self.accumulated_completion_tokens;

            // Parse content for XML tool calls (fallback)
            let parsed = parse_model_output(&result.content);
            let combined_reasoning: String = {
                let structured: String = result.reasoning_details.join("");
                [structured, parsed.reasoning]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            // Merge: structured tool_calls from API take priority, fallback to XML-parsed
            let mut final_tool_calls = result.tool_calls.clone();
            if final_tool_calls.is_empty() && !parsed.tool_calls.is_empty() {
                final_tool_calls = parsed
                    .tool_calls
                    .iter()
                    .enumerate()
                    .map(|(i, tc)| {
                        let args: serde_json::Map<String, Value> = tc
                            .arguments
                            .iter()
                            .map(|(k, v)| (k.clone(), coerce_arg(v)))
                            .collect();
                        AccumulatedToolCall {
                            id: format!("xml_tc_{}_{}", timestamp_ms(), i),
                            call_type: "function".to_string(),
                            function: crate::core::api::ToolCallFunction {
                                name: tc.name.clone(),
                                arguments: serde_json::to_string(&args).unwrap_or_default(),
                            },
                        }
                    })
                    .collect();
            }

            // Build final content
            let mut final_content = parsed.content.clone();
            if final_content.is_empty() && final_tool_calls.is_empty() && !result.content.is_empty()
            {
                final_content = format!(
                    "[Response truncated — the model's output was cut off mid-tool-call]\n\n{}",
                    &result.content[..result.content.len().min(500)]
                );
            } else if final_content.is_empty()
                && final_tool_calls.is_empty()
                && result.content.is_empty()
            {
                final_content = format!(
                    "[Empty response from API — the model returned nothing{}]",
                    if result.finish_reason.is_empty() {
                        String::new()
                    } else {
                        format!(" (finish_reason: {})", result.finish_reason)
                    }
                );
            }

            // Send final message event
            let _ = event_tx.send(ChatEvent::StreamEnd(FinalMessage {
                content: final_content.clone(),
                reasoning: combined_reasoning,
                tool_calls: final_tool_calls.clone(),
            }));

            // Build history entry
            let mut hist_entry = serde_json::json!({
                "role": "assistant",
                "content": result.content
            });
            if !result.reasoning_details.is_empty() {
                hist_entry["reasoning_details"] = serde_json::json!(
                    result.reasoning_details.iter().map(|t| serde_json::json!({"text": t})).collect::<Vec<_>>()
                );
            }
            let sanitized_tool_calls: Vec<AccumulatedToolCall> = final_tool_calls
                .iter()
                .cloned()
                .map(|mut tc| {
                    if serde_json::from_str::<Value>(&tc.function.arguments).is_err() {
                        tc.function.arguments = "{}".to_string();
                    }
                    tc
                })
                .collect();

            if !sanitized_tool_calls.is_empty() {
                hist_entry["tool_calls"] = serde_json::json!(
                    sanitized_tool_calls.iter().map(|tc| serde_json::json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.function.name,
                            "arguments": tc.function.arguments
                        }
                    })).collect::<Vec<_>>()
                );
            }
            self.history.push(hist_entry);
            let tool_calls_json = if sanitized_tool_calls.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&sanitized_tool_calls).unwrap_or_default())
            };
            self.persist_message(
                "assistant",
                &final_content,
                tool_calls_json.as_deref(),
                None,
                None,
            );

            // Execute tool calls if any
            if !final_tool_calls.is_empty() {
                if self.cancel_token.is_cancelled() {
                    break;
                }

                // Parse all args upfront
                let parsed_args: Vec<Value> = final_tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::json!({}))
                    })
                    .collect();

                // Pre-allocate results indexed by position
                let tool_count = final_tool_calls.len();
                let mut results: Vec<Option<(String, String, tools::ToolExecutionResult)>> =
                    (0..tool_count).map(|_| None).collect();

                // Separate intercepted tools (ask_user, todo_write) from regular tools
                let mut regular_indices: Vec<usize> = Vec::new();

                for (i, tc) in final_tool_calls.iter().enumerate() {
                    if tc.function.name == "todo_write" {
                        // Handle todo_write: parse items and send update to UI
                        let args = &parsed_args[i];
                        let items: Vec<TodoItem> = args
                            .get("todos")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|item| {
                                        let content = item.get("content")?.as_str()?.to_string();
                                        let status = match item
                                            .get("status")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("pending")
                                        {
                                            "in_progress" => TodoStatus::InProgress,
                                            "completed" => TodoStatus::Completed,
                                            _ => TodoStatus::Pending,
                                        };
                                        Some(TodoItem { content, status })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        let item_count = items.len();
                        let completed = items
                            .iter()
                            .filter(|t| t.status == TodoStatus::Completed)
                            .count();

                        let _ = event_tx.send(ChatEvent::ToolExecutionStart {
                            id: tc.id.clone(),
                            name: "todo_write".to_string(),
                        });

                        // Send the todo update to the UI
                        let _ = event_tx.send(ChatEvent::TodoUpdate(items));

                        let result_msg = format!(
                            "Todo list updated ({}/{} completed){}",
                            completed, item_count,
                            tools::system_reminder(self.mode, "todo_write")
                        );

                        let _ = event_tx.send(ChatEvent::ToolExecutionDone {
                            id: tc.id.clone(),
                            name: "todo_write".to_string(),
                            result: result_msg.clone(),
                        });

                        results[i] = Some((
                            tc.id.clone(),
                            tc.function.name.clone(),
                            tools::ToolExecutionResult::text(result_msg),
                        ));
                    } else if tc.function.name == "ask_user" {
                        // Handle ask_user synchronously
                        let args = &parsed_args[i];

                        // Parse questions: support both new `questions` array and legacy single-question format
                        let questions: Vec<AgentQuestion> = if let Some(arr) = args.get("questions").and_then(|v| v.as_array()) {
                            arr.iter().enumerate().map(|(qi, item)| {
                                let header = item.get("header")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(&format!("Q{}", qi + 1))
                                    .to_string();
                                let question_text = item.get("question")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("What would you like to do?")
                                    .to_string();
                                let options: Vec<String> = item.get("options")
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                    .unwrap_or_default();
                                let allow_custom = item.get("allow_custom")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(true);
                                AgentQuestion { header, question: question_text, options, allow_custom }
                            }).collect()
                        } else {
                            // Legacy single-question format
                            let question_text = args.get("question")
                                .and_then(|v| v.as_str())
                                .unwrap_or("What would you like to do?")
                                .to_string();
                            let options: Vec<String> = args.get("options")
                                .and_then(|v| v.as_array())
                                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                .unwrap_or_default();
                            let allow_custom = args.get("allow_custom")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(true);
                            vec![AgentQuestion {
                                header: "Question".to_string(),
                                question: question_text,
                                options,
                                allow_custom,
                            }]
                        };

                        let batch = AgentQuestionBatch { questions: questions.clone() };

                        // Create oneshot channel for the response
                        let (resp_tx, resp_rx) = oneshot::channel::<String>();

                        let _ = event_tx.send(ChatEvent::ToolExecutionStart {
                            id: tc.id.clone(),
                            name: "ask_user".to_string(),
                        });

                        // Send the batch to the UI
                        let _ = event_tx.send(ChatEvent::AskUser {
                            batch,
                            response_tx: ResponseChannel(Arc::new(Mutex::new(Some(resp_tx)))),
                        });

                        // Wait for the user's response (blocks this task)
                        let user_answer = tokio::select! {
                            result = resp_rx => {
                                match result {
                                    Ok(answer) => answer,
                                    Err(_) => "No response (cancelled)".to_string(),
                                }
                            }
                            _ = self.cancel_token.cancelled() => {
                                "Cancelled by user".to_string()
                            }
                        };

                        let _ = event_tx.send(ChatEvent::ToolExecutionDone {
                            id: tc.id.clone(),
                            name: "ask_user".to_string(),
                            result: user_answer.clone(),
                        });

                        // Format result: single question uses simple format, multi uses structured
                        let reminder = tools::system_reminder(self.mode, "ask_user");
                        let result_text = if questions.len() == 1 {
                            format!("User responded: {}{}", user_answer, reminder)
                        } else {
                            format!("User responded to {} questions:\n{}{}", questions.len(), user_answer, reminder)
                        };

                        results[i] = Some((
                            tc.id.clone(),
                            tc.function.name.clone(),
                            tools::ToolExecutionResult::text(result_text),
                        ));
                    } else if tc.function.name == "sub_agent" {
                        // Handle sub_agent: launch a nested ChatEngine
                        if self.is_sub_agent {
                            // Prevent recursive sub-agents
                            let _ = event_tx.send(ChatEvent::ToolExecutionStart {
                                id: tc.id.clone(),
                                name: "sub_agent".to_string(),
                            });
                            let err_msg = format!(
                                "Error: Sub-agents cannot spawn sub-agents.{}",
                                tools::system_reminder(self.mode, "sub_agent")
                            );
                            let _ = event_tx.send(ChatEvent::ToolExecutionDone {
                                id: tc.id.clone(),
                                name: "sub_agent".to_string(),
                                result: err_msg.clone(),
                            });
                            results[i] = Some((
                                tc.id.clone(),
                                tc.function.name.clone(),
                                tools::ToolExecutionResult::text(err_msg),
                            ));
                        } else {
                            let args = &parsed_args[i];
                            let task = args
                                .get("task")
                                .and_then(|v| v.as_str())
                                .unwrap_or("No task specified")
                                .to_string();
                            let max_turns_val = args
                                .get("max_turns")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(10)
                                .min(20) as usize;

                            let _ = event_tx.send(ChatEvent::ToolExecutionStart {
                                id: tc.id.clone(),
                                name: "sub_agent".to_string(),
                            });

                            // Create sub-engine
                            let mut sub_engine =
                                ChatEngine::new(self.client.clone(), &self.model, Mode::Plan);
                            sub_engine.set_max_turns(max_turns_val);
                            sub_engine.set_sub_agent(true);
                            sub_engine.set_cancel_token(self.cancel_token.clone());

                            // Forward sub-agent tool events as SubAgentProgress
                            let (sub_tx, mut sub_rx) =
                                mpsc::unbounded_channel::<ChatEvent>();
                            let parent_tx = event_tx.clone();
                            let parent_id = tc.id.clone();
                            let forward_handle = tokio::spawn(async move {
                                while let Some(evt) = sub_rx.recv().await {
                                    match evt {
                                        ChatEvent::ToolExecutionStart { name, .. } => {
                                            let _ = parent_tx.send(
                                                ChatEvent::SubAgentProgress {
                                                    id: parent_id.clone(),
                                                    tool_name: name,
                                                    done: false,
                                                    error: false,
                                                },
                                            );
                                        }
                                        ChatEvent::ToolExecutionDone {
                                            name, result, ..
                                        } => {
                                            let _ = parent_tx.send(
                                                ChatEvent::SubAgentProgress {
                                                    id: parent_id.clone(),
                                                    tool_name: name,
                                                    done: true,
                                                    error: result.starts_with("Error:"),
                                                },
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                            });

                            // Run sub-agent
                            let _sub_result =
                                sub_engine.send_message(&task, None, sub_tx).await;
                            let _ = forward_handle.await;

                            // Extract result + accumulate tokens
                            let answer = sub_engine
                                .last_assistant_content()
                                .unwrap_or_else(|| "(no output)".to_string());
                            self.accumulated_completion_tokens +=
                                sub_engine.accumulated_completion_tokens;

                            let result_msg = format!(
                                "Sub-agent findings:\n\n{}{}",
                                answer,
                                tools::system_reminder(self.mode, "sub_agent")
                            );

                            let _ = event_tx.send(ChatEvent::ToolExecutionDone {
                                id: tc.id.clone(),
                                name: "sub_agent".to_string(),
                                result: result_msg.clone(),
                            });

                            results[i] = Some((
                                tc.id.clone(),
                                tc.function.name.clone(),
                                tools::ToolExecutionResult::text(result_msg),
                            ));
                        }
                    } else {
                        regular_indices.push(i);
                    }
                }

                // Execute remaining regular tools in parallel
                let mut handles: Vec<(usize, tokio::task::JoinHandle<(String, String, tools::ToolExecutionResult)>)> = Vec::new();
                for idx in regular_indices {
                    let tc = &final_tool_calls[idx];
                    let _ = event_tx.send(ChatEvent::ToolExecutionStart {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                    });

                    let name = tc.function.name.clone();
                    let id = tc.id.clone();
                    let args = parsed_args[idx].clone();
                    let mode = self.mode;
                    let mcp = self.mcp_manager.clone();
                    let tx = event_tx.clone();
                    let cancel = self.cancel_token.clone();

                    handles.push((idx, tokio::spawn(async move {
                        if cancel.is_cancelled() {
                            let result = tools::ToolExecutionResult::text("Cancelled".to_string());
                            let _ = tx.send(ChatEvent::ToolExecutionDone {
                                id: id.clone(),
                                name: name.clone(),
                                result: result.result.clone(),
                            });
                            return (id, name, result);
                        }
                        // Route MCP tools to the MCP manager
                        let result = if name.starts_with("mcp__") {
                            if let Some(mcp) = mcp {
                                let manager = mcp.lock().await;
                                match manager.call_tool(&name, args).await {
                                    Ok(result) => tools::ToolExecutionResult::text(result),
                                    Err(e) => tools::ToolExecutionResult::text(
                                        format!("Error: MCP tool failed: {}", e),
                                    ),
                                }
                            } else {
                                tools::ToolExecutionResult::text(
                                    format!("Error: MCP tool \"{}\" called but no MCP manager available", name),
                                )
                            }
                        } else {
                            tools::execute_tool(&name, args, mode).await
                        };

                        // Emit done event immediately when this tool finishes
                        let _ = tx.send(ChatEvent::ToolExecutionDone {
                            id: id.clone(),
                            name: name.clone(),
                            result: result.result.clone(),
                        });

                        (id, name, result)
                    })));
                }

                // Collect parallel results
                for (idx, handle) in handles {
                    let (id, name, result) = match handle.await {
                        Ok(r) => r,
                        Err(e) => {
                            let err_result = tools::ToolExecutionResult::text(format!("Error: {}", e));
                            (String::new(), String::new(), err_result)
                        }
                    };
                    results[idx] = Some((id, name, result));
                }

                // Flatten results in original order
                let ordered_results: Vec<(String, String, tools::ToolExecutionResult)> = results
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| {
                        r.unwrap_or_else(|| {
                            let tc = &final_tool_calls[i];
                            (
                                tc.id.clone(),
                                tc.function.name.clone(),
                                tools::ToolExecutionResult::text("Error: tool result missing".to_string()),
                            )
                        })
                    })
                    .collect();

                // Update history for each tool result
                for (id, _name, result) in &ordered_results {
                    self.history.push(serde_json::json!({
                        "role": "tool",
                        "content": result.result,
                        "tool_call_id": id
                    }));
                }

                // Persist all tool messages
                for (i, tc) in final_tool_calls.iter().enumerate() {
                    if let Some((_, _, result)) = ordered_results.get(i) {
                        self.persist_message(
                            "tool",
                            &result.result,
                            None,
                            Some(&tc.id),
                            Some(&tc.function.name),
                        );
                    }
                }

                // Check cancel after tools complete
                if self.cancel_token.is_cancelled() {
                    break;
                }

                // Continue the loop — model will process tool results
                continue;
            }

            // No tool calls — we're done
            break;
        }

        Ok(())
    }

    fn persist_message(
        &self,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        name: Option<&str>,
    ) {
        if let (Some(session_id), Some(store)) = (&self.session_id, &self.session_store) {
            let _ = store.save_message(session_id, role, content, tool_calls, tool_call_id, name);
        }
    }
}

/// Collapse a tool result to a compact one-line summary, preserving structural
/// metadata like file paths, line numbers, and key outcomes.
fn collapse_tool_result(tool_name: &str, content: &str) -> String {
    match tool_name {
        "read_file" => {
            // Preserve the file path and line count
            let line_count = content.lines().count();
            let first_line = content.lines().next().unwrap_or("");
            format!(
                "[read_file: {} lines, starts with: {}]",
                line_count,
                &first_line[..first_line.len().min(100)]
            )
        }
        "glob" => {
            // Preserve all file paths (they're compact and structurally important)
            let paths: Vec<&str> = content.lines().take(20).collect();
            let total = content.lines().count();
            if total > 20 {
                format!(
                    "{}\n...and {} more paths",
                    paths.join("\n"),
                    total - 20
                )
            } else {
                content.to_string()
            }
        }
        "grep" => {
            // Keep file:line references, drop matched content
            let summaries: Vec<String> = content
                .lines()
                .take(15)
                .map(|line| {
                    // Preserve "path:line:" prefix, truncate content
                    if let Some(colon_pos) = line.find(':') {
                        if let Some(second_colon) = line[colon_pos + 1..].find(':') {
                            let prefix_end = colon_pos + 1 + second_colon + 1;
                            if prefix_end < line.len() {
                                return format!(
                                    "{}...",
                                    &line[..prefix_end.min(line.len())]
                                );
                            }
                        }
                    }
                    line[..line.len().min(80)].to_string()
                })
                .collect();
            let total = content.lines().count();
            let mut result = summaries.join("\n");
            if total > 15 {
                result.push_str(&format!("\n...and {} more matches", total - 15));
            }
            result
        }
        "edit_file" | "write_file" => {
            // Extract file path from the result message
            let first_line = content.lines().next().unwrap_or(content);
            first_line[..first_line.len().min(200)].to_string()
        }
        "bash" => {
            // Keep first and last few lines, drop the middle
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() <= 5 {
                content.to_string()
            } else {
                let head: Vec<&str> = lines.iter().take(3).copied().collect();
                let tail: Vec<&str> = lines.iter().rev().take(2).rev().copied().collect();
                format!(
                    "{}\n... [{} lines omitted] ...\n{}",
                    head.join("\n"),
                    lines.len() - 5,
                    tail.join("\n")
                )
            }
        }
        _ => {
            // Generic: keep first 200 chars
            let safe_end = content
                .char_indices()
                .nth(200)
                .map(|(idx, _)| idx)
                .unwrap_or(content.len());
            format!("{}... [truncated from {} chars]", &content[..safe_end], content.len())
        }
    }
}

fn timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_plan_mode() {
        let client = MiniMaxClient::new("test");
        let engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Plan);
        let prompt = engine.get_system_prompt();
        assert!(prompt.contains("READ-ONLY mode"));
        assert!(prompt.contains("read_file"));
        assert!(!prompt.contains("edit_file"));
    }

    #[test]
    fn system_prompt_builder_mode() {
        let client = MiniMaxClient::new("test");
        let engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);
        let prompt = engine.get_system_prompt();
        assert!(prompt.contains("TOOL USAGE"));
        assert!(prompt.contains("edit_file"));
    }

    #[test]
    fn clear_resets_state() {
        let client = MiniMaxClient::new("test");
        let mut engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);
        engine.history.push(serde_json::json!({"role": "user", "content": "hi"}));
        engine.total_tokens = 1000;

        engine.clear();
        assert!(engine.history.is_empty());
        assert_eq!(engine.total_tokens, 0);
    }

    #[test]
    fn build_full_history_includes_system() {
        let client = MiniMaxClient::new("test");
        let mut engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);
        engine
            .history
            .push(serde_json::json!({"role": "user", "content": "hello"}));

        let full = engine.build_full_history();
        assert_eq!(full.len(), 2);
        assert_eq!(full[0]["role"], "system");
        assert_eq!(full[1]["role"], "user");
    }

    #[test]
    fn estimate_tokens_basic() {
        let client = MiniMaxClient::new("test");
        let mut engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);
        // 400 chars ≈ 100 tokens
        engine
            .history
            .push(serde_json::json!({"role": "user", "content": "a".repeat(400)}));
        assert_eq!(engine.estimate_history_tokens(), 100);
    }

    #[test]
    fn estimate_tokens_empty_history() {
        let client = MiniMaxClient::new("test");
        let engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);
        assert_eq!(engine.estimate_history_tokens(), 0);
    }

    #[test]
    fn build_full_history_strips_old_reasoning() {
        let client = MiniMaxClient::new("test");
        let mut engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);

        // Add 10 messages to ensure some are "old" (threshold = len - 6)
        for i in 0..5 {
            engine.history.push(serde_json::json!({
                "role": "user",
                "content": format!("question {}", i)
            }));
            engine.history.push(serde_json::json!({
                "role": "assistant",
                "content": format!("answer {}", i),
                "reasoning_details": [{"text": "long reasoning here"}]
            }));
        }

        let full = engine.build_full_history();
        // First assistant message (index 2, old) should have reasoning stripped
        assert!(full[2].get("reasoning_details").is_none());
        // Last assistant message (index 10, recent) should keep reasoning
        assert!(full[10].get("reasoning_details").is_some());
    }

    #[test]
    fn build_full_history_truncates_old_tool_results() {
        let client = MiniMaxClient::new("test");
        let mut engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);

        // Add enough messages so the tool result is "old"
        let long_content = "x".repeat(5000);
        engine.history.push(serde_json::json!({
            "role": "tool",
            "content": long_content,
            "tool_call_id": "tc_1"
        }));
        // Add 6 more recent messages to push the tool result past the threshold
        for _ in 0..6 {
            engine.history.push(serde_json::json!({
                "role": "user",
                "content": "msg"
            }));
        }

        let full = engine.build_full_history();
        // The tool result (index 1 in full, after system) should be truncated
        let tool_content = full[1]["content"].as_str().unwrap();
        assert!(tool_content.len() < 5000);
        assert!(tool_content.contains("[truncated"));
    }

    #[test]
    fn build_full_history_keeps_recent_tool_results_intact() {
        let client = MiniMaxClient::new("test");
        let mut engine = ChatEngine::new(client, "MiniMax-M2.5", Mode::Builder);

        let long_content = "x".repeat(5000);
        // Add as a recent message (within the last 6)
        engine.history.push(serde_json::json!({
            "role": "tool",
            "content": long_content,
            "tool_call_id": "tc_1"
        }));

        let full = engine.build_full_history();
        let tool_content = full[1]["content"].as_str().unwrap();
        assert_eq!(tool_content.len(), 5000);
    }
}
