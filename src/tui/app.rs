use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::settings::{save_config, AppConfig};
use crate::core::api::{AccumulatedToolCall, MiniMaxClient, QuotaInfo};
use crate::core::chat::{ChatEngine, ChatEvent, ResponseChannel, TodoItem};
use crate::core::commands::{handle_command, CommandResult};
use crate::core::mcp::McpManager;
use crate::core::session::SessionStore;
use crate::core::Mode;
use crate::tui::agent_question::{self, AgentQuestionState, QuestionAction};
use crate::tui::api_key_prompt::{self, ApiKeyAction, ApiKeyPromptState};
use crate::tui::command_palette::{self, CommandPaletteState, PaletteAction};
use crate::tui::config_menu::{self, ConfigAction, ConfigMenuState};
use crate::tui::file_picker::{self, FilePickerAction, FilePickerState};
use crate::tui::layout as tui_layout;

// ── Token limit constants ──────────────────────────────────────────────

const TOKEN_WARNING_THRESHOLD: u64 = 180_000;
const TOKEN_LIMIT: u64 = 200_000;
const SYSTEM_MESSAGE_TTL_SECONDS: u64 = 10;

// ── System message types ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SystemMessageType {
    Warning,
    Update,
}

// ── Display message types ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: MessageRole,
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<AccumulatedToolCall>,
    pub is_streaming: bool,
    pub tool_status: Option<ToolStatus>,
    pub tool_name: Option<String>,
    /// Progress of tools used inside a sub_agent call.
    pub sub_tools: Vec<(String, ToolStatus)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolStatus {
    Running,
    Done,
    Error,
}

// ── Screen state ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum AppScreen {
    Chat,
    ApiKeyPrompt,
    ConfigMenu,
}

// ── Overlay state ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    CommandPalette,
    FilePicker,
    SessionList { selected: usize },
    AgentQuestion,
}

// ── Application state ───────────────────────────────────────────────────

pub struct App {
    pub config: AppConfig,
    pub mode: Mode,
    pub messages: Vec<DisplayMessage>,
    pub input_text: String,
    pub input_cursor: usize,
    pub scroll_offset: u16,
    pub total_tokens: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub quota: Option<QuotaInfo>,
    pub session_name: String,
    pub screen: AppScreen,
    pub overlay: Overlay,
    pub system_message: Option<String>,
    pub system_message_type: SystemMessageType,
    pub is_streaming: bool,
    pub should_quit: bool,
    pub tick: u64,

    // Overlay states
    pub palette_state: CommandPaletteState,
    pub file_picker_state: FilePickerState,
    pub config_menu_state: ConfigMenuState,
    pub api_key_state: ApiKeyPromptState,
    pub agent_question_state: Option<AgentQuestionState>,
    pub todo_items: Vec<TodoItem>,

    // Internal
    agent_question_tx: Option<ResponseChannel>,
    engine: Option<ChatEngine>,
    session_store: Option<Arc<SessionStore>>,
    session_id: Option<String>,
    chat_event_rx: Option<mpsc::UnboundedReceiver<ChatEvent>>,
    engine_return_rx: Option<oneshot::Receiver<ChatEngine>>,
    quota_refresh_rx: Option<oneshot::Receiver<Result<QuotaInfo, String>>>,
    update_check_rx: Option<oneshot::Receiver<Option<String>>>,
    system_message_expires_at: Option<Instant>,
    cancel_token: CancellationToken,
    #[allow(dead_code)]
    mcp_manager: Option<Arc<tokio::sync::Mutex<McpManager>>>,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let needs_api_key = config.api_key.is_empty()
            && std::env::var("MINIMAX_API_KEY").unwrap_or_default().is_empty();

        Self {
            mode: Mode::Builder,
            messages: Vec::new(),
            input_text: String::new(),
            input_cursor: 0,
            scroll_offset: 0,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            quota: None,
            session_name: "New Session".to_string(),
            screen: if needs_api_key {
                AppScreen::ApiKeyPrompt
            } else {
                AppScreen::Chat
            },
            overlay: Overlay::None,
            system_message: None,
            system_message_type: SystemMessageType::Warning,
            is_streaming: false,
            should_quit: false,
            tick: 0,
            palette_state: CommandPaletteState::new(),
            file_picker_state: FilePickerState::new(),
            config_menu_state: ConfigMenuState::new(),
            api_key_state: ApiKeyPromptState::new(),
            agent_question_state: None,
            todo_items: Vec::new(),
            agent_question_tx: None,
            engine: None,
            session_store: None,
            session_id: None,
            chat_event_rx: None,
            engine_return_rx: None,
            quota_refresh_rx: None,
            update_check_rx: None,
            system_message_expires_at: None,
            cancel_token: CancellationToken::new(),
            mcp_manager: None,
            config,
        }
    }

    /// Initialize the chat engine and session store.
    pub async fn initialize(&mut self) -> Result<()> {
        // Try env var fallback for API key
        if self.config.api_key.is_empty() {
            if let Ok(key) = std::env::var("MINIMAX_API_KEY") {
                self.config.api_key = key;
                self.screen = AppScreen::Chat;
            }
        }

        if self.config.api_key.is_empty() {
            self.screen = AppScreen::ApiKeyPrompt;
            return Ok(());
        }

        self.init_engine().await
    }

    pub async fn init_engine(&mut self) -> Result<()> {
        let client = MiniMaxClient::new(&self.config.api_key);
        self.start_quota_refresh();
        let mut engine = ChatEngine::new(client, &self.config.model, self.mode);

        // Initialize session store
        if let Ok(store) = SessionStore::open() {
            let store = Arc::new(store);
            if let Ok(session) = store.create_session(&self.config.model) {
                self.session_id = Some(session.id.clone());
                self.session_name = session.name.clone();
                engine.set_session(session.id, store.clone());
            }
            self.session_store = Some(store);
        }

        // Initialize MCP servers if configured
        if !self.config.mcp_servers.is_empty() {
            let mut mcp_manager = McpManager::new();
            let tools = mcp_manager.init_servers(&self.config.mcp_servers).await;
            if !tools.is_empty() {
                self.set_system_message(format!(
                    "Connected {} MCP tool(s): {}",
                    tools.len(),
                    tools.join(", ")
                ));
            }
            let mcp_arc = Arc::new(tokio::sync::Mutex::new(mcp_manager));
            engine.set_mcp_manager(mcp_arc.clone());
            self.mcp_manager = Some(mcp_arc);
        }

        self.engine = Some(engine);
        self.start_update_check();
        Ok(())
    }

    pub fn theme_name(&self) -> &str {
        &self.config.theme
    }

    /// Handle a terminal event (key, mouse, resize).
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Only handle key press events — ignore Release/Repeat to avoid duplicates on Windows
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Global: Ctrl+C to quit
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.is_streaming {
                self.cancel_streaming();
            } else {
                self.should_quit = true;
            }
            return;
        }

        // Route to current screen
        match self.screen {
            AppScreen::ApiKeyPrompt => self.handle_api_key_key(key),
            AppScreen::ConfigMenu => self.handle_config_menu_key(key),
            AppScreen::Chat => self.handle_chat_key(key),
        }
    }

    fn handle_api_key_key(&mut self, key: KeyEvent) {
        let action = api_key_prompt::handle_key(&mut self.api_key_state, key);
        match action {
            ApiKeyAction::Submit(api_key) => {
                self.config.api_key = api_key;
                let _ = save_config(&self.config);
                self.screen = AppScreen::Chat;
            }
            ApiKeyAction::Quit => {
                self.should_quit = true;
            }
            ApiKeyAction::None => {}
        }
    }

    fn handle_config_menu_key(&mut self, key: KeyEvent) {
        let action = config_menu::handle_key(
            &mut self.config_menu_state,
            key,
            &self.config.api_key,
            &self.config.theme,
            &self.config.model,
        );
        match action {
            ConfigAction::Close => {
                self.screen = AppScreen::Chat;
            }
            ConfigAction::SetApiKey(api_key) => {
                self.config.api_key = api_key;
                let _ = save_config(&self.config);
                self.screen = AppScreen::Chat;
                self.set_system_message("API key updated.");
                self.engine = None; // Will be re-initialized
            }
            ConfigAction::SetTheme(theme) => {
                self.config.theme = theme.clone();
                let _ = save_config(&self.config);
                self.screen = AppScreen::Chat;
                self.set_system_message(format!("Theme changed to {}", theme));
            }
            ConfigAction::SetModel(model) => {
                self.config.model = model.clone();
                if let Some(engine) = &mut self.engine {
                    engine.set_model(&model);
                }
                let _ = save_config(&self.config);
                self.screen = AppScreen::Chat;
                self.set_system_message(format!("Model changed to {}", model));
            }
            ConfigAction::None => {}
        }
    }

    fn handle_chat_key(&mut self, key: KeyEvent) {
        // Handle overlay input first
        if self.overlay != Overlay::None {
            self.handle_overlay_key(key);
            return;
        }

        // Escape: cancel streaming or clear system message
        if key.code == KeyCode::Esc {
            if self.is_streaming {
                self.cancel_streaming();
            } else if self.system_message.is_some() {
                self.clear_system_message();
            }
            return;
        }

        // Tab: toggle mode
        if key.code == KeyCode::Tab {
            self.toggle_mode();
            return;
        }

        // Scrolling
        match key.code {
            KeyCode::Up => {
                if key.modifiers.contains(KeyModifiers::NONE) && self.input_text.is_empty() {
                    self.scroll_up(5);
                    return;
                }
            }
            KeyCode::Down => {
                if key.modifiers.contains(KeyModifiers::NONE) && self.input_text.is_empty() {
                    self.scroll_down(5);
                    return;
                }
            }
            _ => {}
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('u') => {
                    self.scroll_up(20);
                    return;
                }
                KeyCode::Char('d') => {
                    self.scroll_down(20);
                    return;
                }
                _ => {}
            }
        }

        // Input handling
        match key.code {
            KeyCode::Enter => {
                if !self.is_streaming {
                    self.submit_input();
                }
            }
            KeyCode::Char(c) => {
                self.input_text.insert(self.input_cursor, c);
                self.input_cursor += c.len_utf8();

                // Check for '/' at start of input → open command palette
                if self.input_text == "/" {
                    self.input_text.clear();
                    self.input_cursor = 0;
                    self.palette_state = CommandPaletteState::new();
                    self.overlay = Overlay::CommandPalette;
                }
                // Check for '@' → open file picker
                else if c == '@' {
                    self.file_picker_state = FilePickerState::new();
                    self.overlay = Overlay::FilePicker;
                    // Remove the '@' we just inserted
                    self.input_cursor -= 1;
                    self.input_text.remove(self.input_cursor);
                }
            }
            KeyCode::Backspace => {
                if self.input_cursor > 0 {
                    // Find the previous char boundary
                    let prev = self.input_text[..self.input_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input_text.remove(prev);
                    self.input_cursor = prev;
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input_text.len() {
                    self.input_text.remove(self.input_cursor);
                }
            }
            KeyCode::Left => {
                if self.input_cursor > 0 {
                    // Move to previous char boundary
                    self.input_cursor = self.input_text[..self.input_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            KeyCode::Right => {
                if self.input_cursor < self.input_text.len() {
                    // Move to next char boundary
                    self.input_cursor = self.input_text[self.input_cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input_cursor + i)
                        .unwrap_or(self.input_text.len());
                }
            }
            KeyCode::Home => {
                self.input_cursor = 0;
            }
            KeyCode::End => {
                self.input_cursor = self.input_text.len();
            }
            _ => {}
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) {
        match self.overlay.clone() {
            Overlay::CommandPalette => {
                let action = command_palette::handle_key(&mut self.palette_state, key);
                match action {
                    PaletteAction::Close => {
                        self.overlay = Overlay::None;
                    }
                    PaletteAction::Execute(cmd) => {
                        self.overlay = Overlay::None;
                        let result = handle_command(&cmd);
                        self.apply_command_result(result);
                    }
                    PaletteAction::SetTheme(theme) => {
                        self.overlay = Overlay::None;
                        self.config.theme = theme.clone();
                        let _ = save_config(&self.config);
                        self.set_system_message(format!("Theme changed to {}", theme));
                    }
                    PaletteAction::SetModel(model) => {
                        self.overlay = Overlay::None;
                        self.config.model = model.clone();
                        if let Some(engine) = &mut self.engine {
                            engine.set_model(&model);
                        }
                        let _ = save_config(&self.config);
                        self.set_system_message(format!("Model changed to {}", model));
                    }
                    PaletteAction::None => {}
                }
            }
            Overlay::FilePicker => {
                let action = file_picker::handle_key(&mut self.file_picker_state, key);
                match action {
                    FilePickerAction::Close => {
                        self.overlay = Overlay::None;
                    }
                    FilePickerAction::Select(path) => {
                        self.overlay = Overlay::None;
                        let insert = format!("@{} ", path);
                        self.input_text.insert_str(self.input_cursor, &insert);
                        self.input_cursor += insert.len();
                    }
                    FilePickerAction::TabComplete(_) => {}
                    FilePickerAction::None => {}
                }
            }
            Overlay::SessionList { mut selected } => {
                if key.code == KeyCode::Esc {
                    self.overlay = Overlay::None;
                    return;
                }
                let sessions = self.list_sessions();
                match key.code {
                    KeyCode::Up => {
                        if selected > 0 {
                            selected -= 1;
                        }
                        self.overlay = Overlay::SessionList { selected };
                    }
                    KeyCode::Down => {
                        if selected < sessions.len().saturating_sub(1) {
                            selected += 1;
                        }
                        self.overlay = Overlay::SessionList { selected };
                    }
                    KeyCode::Enter => {
                        if let Some(session) = sessions.get(selected) {
                            self.load_session(&session.0.clone());
                        }
                        self.overlay = Overlay::None;
                    }
                    _ => {}
                }
            }
            Overlay::AgentQuestion => {
                if let Some(ref mut state) = self.agent_question_state {
                    let action = agent_question::handle_key(state, key);
                    match action {
                        QuestionAction::SubmitAll(answers) => {
                            // Format the response based on single vs multi
                            let questions = &state.questions;
                            let formatted = if questions.len() == 1 {
                                answers[0].clone()
                            } else {
                                questions
                                    .iter()
                                    .zip(answers.iter())
                                    .map(|(q, a)| format!("- {}: {}", q.header, a))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            };

                            // Send the answer back to the engine
                            if let Some(tx_channel) = self.agent_question_tx.take() {
                                if let Ok(mut guard) = tx_channel.0.lock() {
                                    if let Some(tx) = guard.take() {
                                        let _ = tx.send(formatted);
                                    }
                                }
                            }
                            // Close the overlay
                            self.overlay = Overlay::None;
                            self.agent_question_state = None;
                        }
                        QuestionAction::None => {}
                    }
                }
            }
            Overlay::None => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_up(5),
            MouseEventKind::ScrollDown => self.scroll_down(5),
            _ => {}
        }
    }

    fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    fn scroll_down(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    fn toggle_mode(&mut self) {
        self.mode = self.mode.toggle();
        if let Some(engine) = &mut self.engine {
            engine.set_mode(self.mode);
        }
    }

    fn cancel_streaming(&mut self) {
        self.cancel_token.cancel();
        self.is_streaming = false;
        self.cancel_token = CancellationToken::new();
        // Clean up agent question overlay if open
        if self.overlay == Overlay::AgentQuestion {
            self.overlay = Overlay::None;
            self.agent_question_state = None;
            self.agent_question_tx = None;
        }
    }

    fn submit_input(&mut self) {
        let text = self.input_text.trim().to_string();
        if text.is_empty() {
            return;
        }

        // Check for slash commands
        if text.starts_with('/') {
            let result = handle_command(&text);
            self.input_text.clear();
            self.input_cursor = 0;
            self.apply_command_result(result);
            return;
        }

        // Resolve @file references
        let (clean_text, file_context) = file_picker::resolve_file_references(&text);
        let display_text = clean_text.clone();

        // Add user message to display
        self.messages.push(DisplayMessage {
            role: MessageRole::User,
            content: display_text,
            reasoning: None,
            tool_calls: Vec::new(),
            is_streaming: false,
            tool_status: None,
            tool_name: None,
            sub_tools: Vec::new(),
        });

        // Reset scroll to bottom
        self.scroll_offset = 0;

        // Clear input
        self.input_text.clear();
        self.input_cursor = 0;

        // Start streaming
        self.start_streaming(text, file_context);
    }

    fn start_streaming(&mut self, user_input: String, file_context: Option<String>) {
        let Some(engine) = self.engine.take() else {
            return;
        };

        self.is_streaming = true;
        self.cancel_token = CancellationToken::new();

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        self.chat_event_rx = Some(event_rx);

        // Add placeholder assistant message
        self.messages.push(DisplayMessage {
            role: MessageRole::Assistant,
            content: String::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            is_streaming: true,
            tool_status: None,
            tool_name: None,
            sub_tools: Vec::new(),
        });

        // Spawn the streaming task and return the engine via oneshot
        let (engine_tx, engine_rx) = oneshot::channel();
        self.engine_return_rx = Some(engine_rx);

        let mut engine_owned = engine;
        engine_owned.set_cancel_token(self.cancel_token.clone());
        let file_ctx = file_context;
        tokio::spawn(async move {
            let _ = engine_owned
                .send_message(&user_input, file_ctx.as_deref(), event_tx)
                .await;
            let _ = engine_tx.send(engine_owned);
        });
    }

    /// Poll for chat events from the streaming task.
    pub fn poll_chat_events(&mut self) {
        // Drain any pending chat events
        if self.chat_event_rx.is_some() {
            let mut events = Vec::new();
            let mut disconnected = false;

            if let Some(rx) = &mut self.chat_event_rx {
                loop {
                    match rx.try_recv() {
                        Ok(event) => events.push(event),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
            }

            for event in events {
                self.process_chat_event(event);
            }

            if disconnected {
                self.is_streaming = false;
                self.chat_event_rx = None;
                // Refresh quota in background after streaming completes
                self.start_quota_refresh();
            }
        }

        // Try to recover the engine from the spawned task.
        // This runs every tick so we catch the engine even if the event
        // channel disconnected before the engine oneshot was sent.
        if self.engine.is_none() {
            if let Some(mut rx) = self.engine_return_rx.take() {
                match rx.try_recv() {
                    Ok(engine) => {
                        self.engine = Some(engine);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {
                        // Engine hasn't been returned yet — put receiver back, retry next tick.
                        self.engine_return_rx = Some(rx);
                    }
                    Err(oneshot::error::TryRecvError::Closed) => {
                        // Sender was dropped without sending — engine is lost.
                    }
                }
            }
        }
    }

    /// Poll for a completed background quota refresh.
    pub fn poll_quota(&mut self) {
        if let Some(mut rx) = self.quota_refresh_rx.take() {
            match rx.try_recv() {
                Ok(Ok(q)) => {
                    self.quota = Some(q);
                }
                Ok(Err(e)) => {
                    self.set_system_message(format!("Quota fetch failed: {}", e));
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    self.quota_refresh_rx = Some(rx);
                }
                Err(_) => {}
            }
        }
    }

    fn start_quota_refresh(&mut self) {
        if self.config.api_key.is_empty() || self.quota_refresh_rx.is_some() {
            return;
        }

        let client = MiniMaxClient::new(&self.config.api_key);
        let (tx, rx) = oneshot::channel();
        self.quota_refresh_rx = Some(rx);

        tokio::spawn(async move {
            let result = match tokio::time::timeout(Duration::from_secs(8), client.fetch_quota()).await {
                Ok(inner) => inner.map_err(|e| e.to_string()),
                Err(_) => Err("Quota fetch timed out after 8s".to_string()),
            };
            let _ = tx.send(result);
        });
    }

    fn start_update_check(&mut self) {
        if self.update_check_rx.is_some() {
            return;
        }
        let (tx, rx) = oneshot::channel();
        self.update_check_rx = Some(rx);
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(8),
                crate::core::update::check_for_update(),
            )
            .await
            .unwrap_or(None);
            let _ = tx.send(result);
        });
    }

    /// Poll for a completed background update check.
    /// Shows the update notification once at startup, then never again.
    pub fn poll_update_check(&mut self) {
        if let Some(mut rx) = self.update_check_rx.take() {
            match rx.try_recv() {
                Ok(Some(version)) => {
                    // Only show the update message if no other system message is active
                    // (e.g., MCP connection info, errors). The update_check_rx is consumed
                    // either way so this notification only fires once per session.
                    if self.system_message.is_none() {
                        let msg = if cfg!(target_os = "windows") {
                            format!(
                                "New version v{} available! Run: irm https://raw.githubusercontent.com/ezeoli88/minmax-code/main/install.ps1 | iex",
                                version
                            )
                        } else {
                            format!(
                                "New version v{} available! Run: curl -fsSL https://raw.githubusercontent.com/ezeoli88/minmax-code/main/install.sh | sh",
                                version
                            )
                        };
                        self.system_message = Some(msg);
                        self.system_message_type = SystemMessageType::Update;
                        self.system_message_expires_at =
                            Some(Instant::now() + Duration::from_secs(SYSTEM_MESSAGE_TTL_SECONDS));
                    }
                    // Do NOT put rx back — update check is consumed, won't show again.
                }
                Ok(None) => {
                    // No update available — done.
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Result not ready yet — put it back to retry next tick.
                    self.update_check_rx = Some(rx);
                }
                Err(_) => {
                    // Channel closed — done.
                }
            }
        }
    }

    fn set_system_message(&mut self, msg: impl Into<String>) {
        // Warnings/errors always take priority and overwrite any current message
        // (including update notifications).
        self.system_message = Some(msg.into());
        self.system_message_type = SystemMessageType::Warning;
        self.system_message_expires_at =
            Some(Instant::now() + Duration::from_secs(SYSTEM_MESSAGE_TTL_SECONDS));
    }

    fn clear_system_message(&mut self) {
        self.system_message = None;
        self.system_message_expires_at = None;
    }

    fn poll_system_message_expiry(&mut self) {
        if let Some(expires_at) = self.system_message_expires_at {
            if Instant::now() >= expires_at {
                self.clear_system_message();
            }
        }
    }

    fn process_chat_event(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::StreamStart => {
                if self
                    .messages
                    .last()
                    .map(|m| m.role != MessageRole::Assistant || !m.is_streaming)
                    .unwrap_or(true)
                {
                    self.messages.push(DisplayMessage {
                        role: MessageRole::Assistant,
                        content: String::new(),
                        reasoning: None,
                        tool_calls: Vec::new(),
                        is_streaming: true,
                        tool_status: None,
                        tool_name: None,
                        sub_tools: Vec::new(),
                    });
                }
            }
            ChatEvent::ReasoningChunk(text) => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && m.is_streaming)
                {
                    let r = msg.reasoning.get_or_insert_with(String::new);
                    r.push_str(&text);
                }
            }
            ChatEvent::ContentChunk(text) => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && m.is_streaming)
                {
                    msg.content.push_str(&text);
                }
                // Auto-scroll only if user is already at the bottom
                if self.scroll_offset <= 1 {
                    self.scroll_offset = 0;
                }
            }
            ChatEvent::ToolCallsUpdate(tool_calls) => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && m.is_streaming)
                {
                    msg.tool_calls = tool_calls;
                }
            }
            ChatEvent::StreamEnd(final_msg) => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && m.is_streaming)
                {
                    msg.content = final_msg.content;
                    msg.tool_calls = final_msg.tool_calls;
                    if !final_msg.reasoning.is_empty() {
                        msg.reasoning = Some(final_msg.reasoning);
                    }
                    msg.is_streaming = false;
                }
            }
            ChatEvent::ToolExecutionStart { id: _, name } => {
                self.messages.push(DisplayMessage {
                    role: MessageRole::Tool,
                    content: String::new(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    is_streaming: false,
                    tool_status: Some(ToolStatus::Running),
                    tool_name: Some(name),
                    sub_tools: Vec::new(),
                });
            }
            ChatEvent::ToolExecutionDone {
                id: _,
                name,
                result,
            } => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| {
                        m.role == MessageRole::Tool
                            && m.tool_name.as_deref() == Some(&name)
                            && m.tool_status == Some(ToolStatus::Running)
                    })
                {
                    let is_error = result.starts_with("Error:");
                    msg.content = result;
                    msg.tool_status = Some(if is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Done
                    });
                }
            }
            ChatEvent::TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: _,
            } => {
                // prompt_tokens reflects the full context window each request (system prompt +
                // history + tools), so we take the latest value instead of accumulating.
                // completion_tokens are new tokens generated per request, so we accumulate those.
                self.prompt_tokens = prompt_tokens;
                self.completion_tokens += completion_tokens;
                self.total_tokens = self.prompt_tokens + self.completion_tokens;
                self.check_token_limits();
            }
            ChatEvent::Error(msg) => {
                self.set_system_message(msg);
            }
            ChatEvent::TodoUpdate(items) => {
                self.todo_items = items;
            }
            ChatEvent::AskUser {
                batch,
                response_tx,
            } => {
                // Store the response sender so we can send back the answer
                self.agent_question_tx = Some(response_tx);
                // Create the overlay state and show it
                self.agent_question_state = Some(AgentQuestionState::new(batch));
                self.overlay = Overlay::AgentQuestion;
            }
            ChatEvent::ContextCompressed {
                original_tokens,
                compressed_tokens,
            } => {
                self.set_system_message(format!(
                    "Context compressed: ~{}K → ~{}K tokens",
                    original_tokens / 1000,
                    compressed_tokens / 1000
                ));
            }
            ChatEvent::SubAgentProgress {
                id: _,
                tool_name,
                done,
                error,
            } => {
                // Find the running sub_agent tool message and update its sub_tools
                if let Some(msg) = self.messages.iter_mut().rev().find(|m| {
                    m.role == MessageRole::Tool
                        && m.tool_name.as_deref() == Some("sub_agent")
                        && m.tool_status == Some(ToolStatus::Running)
                }) {
                    if done {
                        // Update the last Running entry for this tool_name to Done/Error
                        if let Some(entry) = msg
                            .sub_tools
                            .iter_mut()
                            .rev()
                            .find(|e| e.0 == tool_name && e.1 == ToolStatus::Running)
                        {
                            entry.1 = if error {
                                ToolStatus::Error
                            } else {
                                ToolStatus::Done
                            };
                        }
                    } else {
                        msg.sub_tools.push((tool_name, ToolStatus::Running));
                    }
                }
            }
        }
    }

    /// Check token limits and show warning or auto-new-session.
    fn check_token_limits(&mut self) {
        if self.total_tokens >= TOKEN_LIMIT {
            self.set_system_message("Token limit reached. Starting new session...");
            self.new_session();
        } else if self.total_tokens >= TOKEN_WARNING_THRESHOLD {
            self.set_system_message(
                "Warning: Approaching token limit. Consider starting a new session with /new"
            );
        }
    }

    fn apply_command_result(&mut self, result: CommandResult) {
        match result {
            CommandResult::Message(msg) => {
                self.messages.push(DisplayMessage {
                    role: MessageRole::System,
                    content: msg,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    is_streaming: false,
                    tool_status: None,
                    tool_name: None,
                    sub_tools: Vec::new(),
                });
            }
            CommandResult::NewSession => {
                self.new_session();
            }
            CommandResult::Clear => {
                self.messages.clear();
                if let Some(engine) = &mut self.engine {
                    engine.clear();
                }
            }
            CommandResult::Exit => {
                self.should_quit = true;
            }
            CommandResult::Sessions => {
                self.overlay = Overlay::SessionList { selected: 0 };
            }
            CommandResult::Config => {
                self.config_menu_state = ConfigMenuState::new();
                self.screen = AppScreen::ConfigMenu;
            }
            CommandResult::SetModel(model) => {
                self.config.model = model.clone();
                if let Some(engine) = &mut self.engine {
                    engine.set_model(&model);
                }
                let _ = save_config(&self.config);
                self.set_system_message(format!("Model changed to {}", model));
            }
            CommandResult::SetTheme(theme) => {
                self.config.theme = theme.clone();
                let _ = save_config(&self.config);
                self.set_system_message(format!("Theme changed to {}", theme));
            }
            CommandResult::None => {}
        }
    }

    fn new_session(&mut self) {
        self.messages.clear();
        self.todo_items.clear();
        self.total_tokens = 0;
        self.prompt_tokens = 0;
        self.completion_tokens = 0;
        self.scroll_offset = 0;

        // Refresh quota in background (account-level, not session-level)
        self.start_quota_refresh();

        if let Some(store) = &self.session_store {
            if let Ok(session) = store.create_session(&self.config.model) {
                self.session_id = Some(session.id.clone());
                self.session_name = session.name.clone();
                if let Some(engine) = &mut self.engine {
                    engine.clear();
                    engine.set_session(session.id, store.clone());
                }
            }
        }
    }

    pub fn list_sessions(&self) -> Vec<(String, String, String)> {
        if let Some(store) = &self.session_store {
            store
                .list_sessions()
                .unwrap_or_default()
                .into_iter()
                .map(|s| (s.id, s.name, s.model))
                .collect()
        } else {
            Vec::new()
        }
    }

    fn load_session(&mut self, session_id: &str) {
        let Some(store) = &self.session_store else {
            return;
        };
        let msgs = store.get_session_messages(session_id).unwrap_or_default();

        self.messages.clear();
        self.session_id = Some(session_id.to_string());

        for msg in &msgs {
            let role = match msg.role.as_str() {
                "user" => MessageRole::User,
                "assistant" => MessageRole::Assistant,
                "tool" => MessageRole::Tool,
                _ => MessageRole::System,
            };
            self.messages.push(DisplayMessage {
                role,
                content: msg.content.clone(),
                reasoning: None,
                tool_calls: Vec::new(),
                is_streaming: false,
                tool_status: if msg.role == "tool" {
                    Some(ToolStatus::Done)
                } else {
                    None
                },
                tool_name: msg.name.clone(),
                sub_tools: Vec::new(),
            });
        }

        // Rebuild engine history
        if let Some(engine) = &mut self.engine {
            engine.clear();
            let history: Vec<serde_json::Value> = msgs
                .iter()
                .map(|m| {
                    let mut v = serde_json::json!({
                        "role": m.role,
                        "content": m.content
                    });
                    if let Some(tc) = &m.tool_calls {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(tc) {
                            v["tool_calls"] = parsed;
                        }
                    }
                    if let Some(id) = &m.tool_call_id {
                        v["tool_call_id"] = serde_json::json!(id);
                    }
                    v
                })
                .collect();
            engine.load_history(history);
            engine.set_session(session_id.to_string(), store.clone());
        }

        self.scroll_offset = 0;
    }

    /// Check if the engine needs to be initialized (after API key is set).
    pub fn needs_engine_init(&self) -> bool {
        self.engine.is_none() && !self.config.api_key.is_empty() && self.screen == AppScreen::Chat
    }
}

/// The main run loop.
pub async fn run(config: AppConfig) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
    )?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config);
    app.initialize().await?;

    let result = event_loop(&mut terminal, &mut app).await;

    // Shutdown MCP servers
    if let Some(mcp) = &app.mcp_manager {
        let mut manager = mcp.lock().await;
        manager.shutdown().await;
    }

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;

    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        // Check if engine needs initialization (after API key prompt)
        if app.needs_engine_init() {
            app.init_engine().await?;
        }

        terminal.draw(|frame| {
            tui_layout::draw(frame, app);
        })?;

        if app.should_quit {
            break;
        }

        app.poll_chat_events();
        app.poll_quota();
        app.poll_update_check();
        app.poll_system_message_expiry();
        app.tick = app.tick.wrapping_add(1);

        if crossterm::event::poll(Duration::from_millis(16))? {
            let event = event::read()?;
            app.handle_event(event);
        }
    }
    Ok(())
}
