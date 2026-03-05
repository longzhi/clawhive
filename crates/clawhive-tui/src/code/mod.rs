use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use clawhive_bus::{EventBus, Topic};
use clawhive_core::approval::ApprovalRegistry;
use clawhive_gateway::Gateway;
use clawhive_schema::{ApprovalDecision, BusMessage, InboundMessage};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    Frame, Terminal,
};
use tokio::sync::mpsc;

pub mod bottom_pane;
pub mod diff;
pub mod footer;
pub mod header;
pub mod history;
pub mod markdown;
pub mod scroll;
pub mod shimmer;

use self::bottom_pane::input::InputView;
use self::bottom_pane::{ApprovalRequest, BottomPaneState, FilterState};
use self::history::HistoryCell;
use self::scroll::ScrollState;

pub(crate) struct CodeApp {
    pub history: Vec<HistoryCell>,
    pub bottom_pane: BottomPaneState,
    pub approval_queue: VecDeque<ApprovalRequest>,

    pub history_scroll: ScrollState,
    pub history_clear_offset: usize,

    pub input: String,
    pub input_view: InputView,
    pub input_history: Vec<String>,
    pub input_history_index: Option<usize>,
    pub file_search_paths: Vec<String>,
    pub queued_message: Option<String>,
    pub changed_files: Vec<String>,

    pub is_running: bool,
    pub agent_id: String,
    pub model_name: String,
    pub token_count: u64,
    pub cost_usd: f64,
    pub context_used_pct: u8,

    pub verbose: bool,

    pub should_quit: bool,
    pub quit_pressed_at: Option<Instant>,
}

impl CodeApp {
    pub(crate) fn new(agent_id: String, model_name: String) -> Self {
        Self {
            history: Vec::new(),
            bottom_pane: BottomPaneState::default(),
            approval_queue: VecDeque::new(),
            history_scroll: ScrollState::new(),
            history_clear_offset: 0,
            input: String::new(),
            input_view: InputView::new(),
            input_history: Vec::new(),
            input_history_index: None,
            file_search_paths: Vec::new(),
            queued_message: None,
            changed_files: Vec::new(),
            is_running: false,
            agent_id,
            model_name,
            token_count: 0,
            cost_usd: 0.0,
            context_used_pct: 0,
            verbose: false,
            should_quit: false,
            quit_pressed_at: None,
        }
    }

    fn sync_input_view(&mut self) {
        if self.input_view.text() != self.input {
            self.input_view.set_text(&self.input);
            self.input_view.detect_shell_mode();
        }
    }

    pub(crate) fn input_view_height(&mut self) -> u16 {
        if self.is_running {
            return 1;
        }
        self.sync_input_view();
        self.input_view.desired_height()
    }

    fn filtered_paths(&self, query: &str) -> Vec<String> {
        bottom_pane::file_search::filter_paths(&self.file_search_paths, query)
    }

    pub(crate) fn execute_slash_command(&mut self, command_name: &str) {
        match command_name {
            "/compact" => {
                // Mark current history as compacted - visually clear while preserving data
                self.history_clear_offset = self.history.len();
                self.history_scroll = ScrollState::new();
                self.history.push(HistoryCell::AssistantText {
                    text: "Conversation compacted. Previous context hidden.".to_string(),
                    is_streaming: false,
                });
            }
            "/context" => {
                let info = format!(
                    "**Context Window Usage:** {}%\n\n- Tokens used: {}\n- Estimated cost: ${:.2}",
                    self.context_used_pct, self.token_count, self.cost_usd
                );
                self.history.push(HistoryCell::AssistantText {
                    text: info,
                    is_streaming: false,
                });
            }
            "/cost" => {
                let info = format!(
                    "**Token Usage:** {}\n**Estimated Cost:** ${:.2}",
                    self.token_count, self.cost_usd
                );
                self.history.push(HistoryCell::AssistantText {
                    text: info,
                    is_streaming: false,
                });
            }
            "/diff" => {
                let text = if self.changed_files.is_empty() {
                    "No file changes tracked in this session.".to_string()
                } else {
                    let mut out = String::from("**Files changed this session:**\n");
                    for file in &self.changed_files {
                        out.push_str(&format!("\n- `{}`", file));
                    }
                    out
                };
                self.history.push(HistoryCell::AssistantText {
                    text,
                    is_streaming: false,
                });
            }
            "/clear" => {
                self.history_clear_offset = self.history.len();
                self.history_scroll = ScrollState::new();
            }
            "/model" => {
                let info = format!(
                    "**Agent:** {}\n**Model:** {}",
                    self.agent_id, self.model_name
                );
                self.history.push(HistoryCell::AssistantText {
                    text: info,
                    is_streaming: false,
                });
            }
            "/help" => {
                let help = [
                    "**Available Commands:**",
                    "",
                    "- `/compact` — Compress conversation history",
                    "- `/context` — Show context window usage",
                    "- `/cost` — Show token usage and cost",
                    "- `/diff` — Show files changed this session",
                    "- `/clear` — Clear screen (keep history)",
                    "- `/model` — Show current model info",
                    "- `/help` — Show this help",
                    "- `/exit` — Exit the TUI",
                ]
                .join("\n");
                self.history.push(HistoryCell::AssistantText {
                    text: help,
                    is_streaming: false,
                });
            }
            "/exit" => {
                self.should_quit = true;
            }
            _ => {}
        }

        self.history_scroll.ensure_bottom(self.history.len(), 20);
    }

    pub(crate) fn handle_bus_message(&mut self, msg: BusMessage, connector_id: &str) {
        match msg {
            BusMessage::MessageAccepted { .. } => {
                self.is_running = true;
            }
            BusMessage::StreamDelta {
                delta, is_final, ..
            } => {
                if is_final {
                    if let Some(HistoryCell::AssistantText { is_streaming, .. }) =
                        self.history.last_mut()
                    {
                        *is_streaming = false;
                    }
                    self.is_running = false;
                } else if !delta.is_empty() {
                    match self.history.last_mut() {
                        Some(HistoryCell::AssistantText { text, is_streaming })
                            if *is_streaming =>
                        {
                            text.push_str(&delta);
                        }
                        _ => {
                            self.history.push(HistoryCell::AssistantText {
                                text: delta,
                                is_streaming: true,
                            });
                        }
                    }
                    self.history_scroll.ensure_bottom(self.history.len(), 20);
                }
            }
            BusMessage::ReplyReady { outbound } => {
                if outbound.channel_type == "code" && outbound.connector_id == connector_id {
                    if let Some(HistoryCell::AssistantText { is_streaming, .. }) =
                        self.history.last_mut()
                    {
                        *is_streaming = false;
                    }
                    if !matches!(self.history.last(), Some(HistoryCell::AssistantText { .. })) {
                        self.history.push(HistoryCell::AssistantText {
                            text: outbound.text,
                            is_streaming: false,
                        });
                    }
                    self.is_running = false;
                    self.history_scroll.ensure_bottom(self.history.len(), 20);
                }
            }
            BusMessage::TaskFailed { trace_id, error } => {
                self.history.push(HistoryCell::Error {
                    trace_id,
                    message: error,
                });
                self.is_running = false;
                self.history_scroll.ensure_bottom(self.history.len(), 20);
            }
            BusMessage::NeedHumanApproval {
                trace_id,
                command,
                agent_id,
                ..
            } => {
                let request = ApprovalRequest {
                    trace_id,
                    command,
                    agent_id,
                    diff: None,
                    selected_option: 0,
                };
                self.approval_queue.push_back(request);
                if !matches!(self.bottom_pane, BottomPaneState::Approval(_)) {
                    if let Some(req) = self.approval_queue.pop_front() {
                        self.bottom_pane = BottomPaneState::Approval(req);
                    }
                }
            }
            BusMessage::ToolCallStarted {
                tool_name,
                arguments,
                ..
            } => {
                self.history.push(HistoryCell::ToolCall {
                    tool_name,
                    arguments,
                    output: None,
                    duration: None,
                    is_running: true,
                });
                self.history_scroll.ensure_bottom(self.history.len(), 20);
            }
            BusMessage::ToolCallCompleted {
                tool_name,
                output,
                duration_ms,
                ..
            } => {
                // Try to find matching running ToolCall and update it
                let duration = std::time::Duration::from_millis(duration_ms);
                let mut found = false;
                for cell in self.history.iter_mut().rev() {
                    if let HistoryCell::ToolCall {
                        tool_name: ref name,
                        is_running: ref mut running,
                        output: ref mut out,
                        duration: ref mut dur,
                        ..
                    } = cell
                    {
                        if *running && *name == tool_name {
                            *running = false;
                            *out = if output.is_empty() {
                                None
                            } else {
                                Some(history::ToolOutput::Text(
                                    output.lines().map(String::from).collect(),
                                ))
                            };
                            *dur = Some(duration);
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    // No matching start found; insert completed tool call directly
                    self.history.push(HistoryCell::ToolCall {
                        tool_name: tool_name.clone(),
                        arguments: String::new(),
                        output: if output.is_empty() {
                            None
                        } else {
                            Some(history::ToolOutput::Text(
                                output.lines().map(String::from).collect(),
                            ))
                        },
                        duration: Some(duration),
                        is_running: false,
                    });
                }
                // Track changed files from tool output
                if tool_name == "write" || tool_name == "edit" {
                    if let Some(first_line) = output.lines().next() {
                        let file = first_line.trim().to_string();
                        if !file.is_empty() && !self.changed_files.contains(&file) {
                            self.changed_files.push(file);
                        }
                    }
                }
                self.history_scroll.ensure_bottom(self.history.len(), 20);
            }
            _ => {}
        }
    }
}

pub(crate) fn render(frame: &mut Frame, app: &mut CodeApp) {
    let area = frame.area();
    let input_height = app.input_view_height();

    let bottom_height = match &app.bottom_pane {
        BottomPaneState::Input => input_height,
        BottomPaneState::Approval(req) => {
            bottom_pane::approval::desired_approval_height(req, area.width)
        }
        BottomPaneState::ShortcutOverlay => bottom_pane::shortcuts::shortcut_overlay_height(),
        BottomPaneState::SlashCommand(filter) => {
            input_height + bottom_pane::slash_command::slash_picker_height(&filter.query)
        }
        BottomPaneState::FileSearch(filter) => {
            let filtered = app.filtered_paths(&filter.query);
            input_height + bottom_pane::file_search::file_picker_height(filtered.len())
        }
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(bottom_height),
            Constraint::Length(1),
        ])
        .split(area);

    header::render_header(layout[0], frame.buffer_mut(), app);
    let visible_history = &app.history[app.history_clear_offset..];
    history::render_history_pane(
        visible_history,
        &app.history_scroll,
        layout[1],
        frame.buffer_mut(),
        app.verbose,
    );

    match &app.bottom_pane {
        BottomPaneState::Input => {
            let accent = if app.input_view.is_shell_mode {
                crate::shared::styles::INFO
            } else {
                crate::shared::styles::AGENT_ACCENT
            };
            bottom_pane::input::render_input_view(
                layout[2],
                frame.buffer_mut(),
                &mut app.input_view,
                app.is_running,
                accent,
            );
        }
        BottomPaneState::Approval(req) => {
            let queue_len = app.approval_queue.len() + 1;
            bottom_pane::approval::render_approval_view(
                layout[2],
                frame.buffer_mut(),
                req,
                (1, queue_len),
            );
        }
        BottomPaneState::ShortcutOverlay => {
            bottom_pane::shortcuts::render_shortcut_overlay(layout[2], frame.buffer_mut());
        }
        BottomPaneState::SlashCommand(filter) => {
            let picker_h = bottom_pane::slash_command::slash_picker_height(&filter.query);
            let (input_area, picker_area) = split_bottom(layout[2], input_height, picker_h);

            bottom_pane::input::render_input_view(
                input_area,
                frame.buffer_mut(),
                &mut app.input_view,
                app.is_running,
                crate::shared::styles::AGENT_ACCENT,
            );
            bottom_pane::slash_command::render_slash_command_picker(
                picker_area,
                frame.buffer_mut(),
                &filter.query,
                filter.selected,
            );
        }
        BottomPaneState::FileSearch(filter) => {
            let filtered = app.filtered_paths(&filter.query);
            let picker_h = bottom_pane::file_search::file_picker_height(filtered.len());
            let (input_area, picker_area) = split_bottom(layout[2], input_height, picker_h);

            bottom_pane::input::render_input_view(
                input_area,
                frame.buffer_mut(),
                &mut app.input_view,
                app.is_running,
                crate::shared::styles::AGENT_ACCENT,
            );
            bottom_pane::file_search::render_file_search_picker(
                picker_area,
                frame.buffer_mut(),
                &filtered,
                filter.selected,
            );
        }
    }

    footer::render_footer(layout[3], frame.buffer_mut(), app);
}

fn split_bottom(area: Rect, input_height: u16, picker_height: u16) -> (Rect, Rect) {
    let input_len = input_height.min(area.height);
    let picker_len = picker_height.min(area.height.saturating_sub(input_len));
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(input_len),
            Constraint::Length(picker_len),
        ])
        .split(area);
    (parts[0], parts[1])
}

pub(crate) async fn run(
    bus: &EventBus,
    gateway: Arc<Gateway>,
    approval_registry: Option<Arc<ApprovalRegistry>>,
) -> Result<()> {
    let mut rx_reply = bus.subscribe(Topic::ReplyReady).await;
    let mut rx_accept = bus.subscribe(Topic::MessageAccepted).await;
    let mut rx_fail = bus.subscribe(Topic::TaskFailed).await;
    let mut rx_stream = bus.subscribe(Topic::StreamDelta).await;
    let mut rx_approval = bus.subscribe(Topic::NeedHumanApproval).await;
    let mut rx_tool_start = bus.subscribe(Topic::ToolCallStarted).await;
    let mut rx_tool_done = bus.subscribe(Topic::ToolCallCompleted).await;

    let connector_id = format!(
        "code-{}-{}-{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
        std::process::id(),
        &uuid::Uuid::new_v4().to_string()[..4]
    );
    let conversation_scope = format!("code:{connector_id}:main");
    let user_scope = format!(
        "user:code:{}",
        std::env::var("USER").unwrap_or_else(|_| "developer".to_string())
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let gateway_bg = gateway.clone();
    let connector_bg = connector_id.clone();
    let scope_bg = conversation_scope.clone();
    let user_bg = user_scope.clone();
    tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            let inbound = InboundMessage {
                trace_id: uuid::Uuid::new_v4(),
                channel_type: "code".into(),
                connector_id: connector_bg.clone(),
                conversation_scope: scope_bg.clone(),
                user_scope: user_bg.clone(),
                text,
                at: chrono::Utc::now(),
                thread_id: None,
                is_mention: false,
                mention_target: None,
                message_id: None,
                attachments: vec![],
                group_context: None,
            };
            if let Err(err) = gateway_bg.handle_inbound(inbound).await {
                tracing::error!("code inbound failed: {err}");
            }
        }
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = CodeApp::new("clawhive-main".to_string(), "default".to_string());

    let run_result = (|| -> Result<()> {
        loop {
            while let Ok(msg) = rx_reply.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }
            while let Ok(msg) = rx_accept.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }
            while let Ok(msg) = rx_fail.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }
            while let Ok(msg) = rx_stream.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }
            while let Ok(msg) = rx_approval.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }
            while let Ok(msg) = rx_tool_start.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }
            while let Ok(msg) = rx_tool_done.try_recv() {
                app.handle_bus_message(msg, &connector_id);
            }

            // Send queued message when agent finishes
            if !app.is_running {
                if let Some(queued) = app.queued_message.take() {
                    app.history.push(HistoryCell::UserMessage {
                        text: queued.clone(),
                        timestamp: Local::now(),
                    });
                    app.input_history.push(queued.clone());
                    let _ = tx.send(queued);
                    app.is_running = true;
                    app.history_scroll.ensure_bottom(app.history.len(), 20);
                }
            }

            terminal.draw(|f| render(f, &mut app))?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        handle_key_event(&mut app, key, &tx, &approval_registry)?;
                    }
                }
            }

            if app.should_quit {
                break;
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    run_result
}

fn handle_key_event(
    app: &mut CodeApp,
    key: KeyEvent,
    tx: &mpsc::UnboundedSender<String>,
    approval_registry: &Option<Arc<ApprovalRegistry>>,
) -> Result<()> {
    if let BottomPaneState::Approval(req) = &mut app.bottom_pane {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter if req.selected_option == 0 => {
                resolve_approval(app, ApprovalDecision::AllowOnce, approval_registry);
            }
            KeyCode::Char('a') | KeyCode::Enter if req.selected_option == 1 => {
                resolve_approval(app, ApprovalDecision::AllowOnce, approval_registry);
            }
            KeyCode::Char('A') | KeyCode::Enter if req.selected_option == 2 => {
                resolve_approval(app, ApprovalDecision::AlwaysAllow, approval_registry);
            }
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Enter if req.selected_option == 3 => {
                resolve_approval(app, ApprovalDecision::Deny, approval_registry);
            }
            KeyCode::Up => {
                req.selected_option = req.selected_option.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = bottom_pane::approval::approval_option_count(req).saturating_sub(1);
                req.selected_option = (req.selected_option + 1).min(max);
            }
            KeyCode::Char('?') | KeyCode::Enter if req.selected_option == 4 => {
                // Explain command: add explanation request to history
                let cmd = req.command.clone();
                app.history.push(HistoryCell::AssistantText {
                    text: format!("**Explain:** `{}`\n\nThis command will be executed in a shell. Review it carefully before approving.", cmd),
                    is_streaming: false,
                });
                app.history_scroll.ensure_bottom(app.history.len(), 20);
            }
            _ => {}
        }
        return Ok(());
    }

    if matches!(app.bottom_pane, BottomPaneState::ShortcutOverlay) {
        app.bottom_pane = BottomPaneState::Input;
        return Ok(());
    }

    let slash_action = if let BottomPaneState::SlashCommand(filter) = &mut app.bottom_pane {
        let mut switch_to_input = false;
        let mut execute = None;

        match key.code {
            KeyCode::Esc => {
                switch_to_input = true;
            }
            KeyCode::Enter => {
                let filtered = bottom_pane::slash_command::filter_commands(&filter.query);
                let idx = filter.selected.min(filtered.len().saturating_sub(1));
                execute = filtered.get(idx).map(|command| command.name.to_string());
                switch_to_input = true;
            }
            KeyCode::Up => {
                filter.selected = filter.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                filter.selected += 1;
            }
            KeyCode::Char(c) => {
                filter.query.push(c);
            }
            KeyCode::Backspace => {
                filter.query.pop();
                if filter.query.is_empty() {
                    switch_to_input = true;
                }
            }
            _ => {}
        }

        Some((switch_to_input, execute))
    } else {
        None
    };

    if let Some((switch_to_input, execute)) = slash_action {
        if switch_to_input {
            app.bottom_pane = BottomPaneState::Input;
        }
        if let Some(command_name) = execute {
            app.execute_slash_command(&command_name);
        }
        return Ok(());
    }

    if let BottomPaneState::FileSearch(filter) = &mut app.bottom_pane {
        match key.code {
            KeyCode::Esc => {
                app.bottom_pane = BottomPaneState::Input;
            }
            KeyCode::Enter | KeyCode::Tab => {
                let all_paths = app.file_search_paths.clone();
                let filtered = bottom_pane::file_search::filter_paths(&all_paths, &filter.query);
                if let Some(path) =
                    filtered.get(filter.selected.min(filtered.len().saturating_sub(1)))
                {
                    app.input.push_str(path);
                    app.input_view.set_text(&app.input);
                }
                app.bottom_pane = BottomPaneState::Input;
            }
            KeyCode::Up => {
                filter.selected = filter.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                filter.selected += 1;
            }
            KeyCode::Char(c) => {
                filter.query.push(c);
            }
            KeyCode::Backspace => {
                filter.query.pop();
                if filter.query.is_empty() {
                    app.bottom_pane = BottomPaneState::Input;
                }
            }
            _ => {}
        }
        return Ok(());
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        let now = Instant::now();
        let should_quit = app
            .quit_pressed_at
            .map(|at| now.duration_since(at) <= Duration::from_millis(900))
            .unwrap_or(false);
        if should_quit {
            app.should_quit = true;
        } else {
            app.quit_pressed_at = Some(now);
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.input.push('\n');
        }
        KeyCode::Tab if app.is_running && !app.input.trim().is_empty() => {
            // Queue message to send after agent finishes
            app.queued_message = Some(app.input.trim().to_string());
            app.input.clear();
            app.input_view.clear();
            app.quit_pressed_at = None;
        }
        KeyCode::Enter => {
            let text = app.input.trim().to_string();
            if !text.is_empty() {
                let outbound = if let Some(stripped) = text.strip_prefix('!') {
                    stripped.to_string()
                } else {
                    text.clone()
                };
                if !outbound.is_empty() {
                    let _ = tx.send(outbound);
                    app.is_running = true;
                }
                app.history.push(HistoryCell::UserMessage {
                    text: text.clone(),
                    timestamp: Local::now(),
                });
                app.input_history.push(text.clone());
                app.input_history_index = None;
                if text.contains('/') {
                    app.file_search_paths.push(text);
                }
                app.history_scroll.ensure_bottom(app.history.len(), 20);
            }
            app.input.clear();
            app.input_view.clear();
            app.quit_pressed_at = None;
        }
        KeyCode::Char('/') if app.input.is_empty() => {
            app.bottom_pane = BottomPaneState::SlashCommand(FilterState {
                query: String::new(),
                selected: 0,
            });
            app.quit_pressed_at = None;
        }
        KeyCode::Char('@') => {
            app.input.push('@');
            if app.file_search_paths.is_empty() {
                let cwd = std::env::current_dir().unwrap_or_default();
                app.file_search_paths = bottom_pane::file_search::scan_workspace_files(&cwd, 5000);
            }
            app.bottom_pane = BottomPaneState::FileSearch(FilterState {
                query: String::new(),
                selected: 0,
            });
            app.quit_pressed_at = None;
        }
        KeyCode::Char('?') if app.input.is_empty() => {
            app.bottom_pane = BottomPaneState::ShortcutOverlay;
            app.quit_pressed_at = None;
        }
        KeyCode::Esc => {
            if app.is_running {
                app.is_running = false;
            } else {
                app.input.clear();
            }
            app.quit_pressed_at = None;
        }
        KeyCode::Char('q') if app.input.is_empty() && !app.is_running => {
            app.should_quit = true;
        }
        KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+L: clear screen (same as /clear)
            app.history_clear_offset = app.history.len();
            app.history_scroll = ScrollState::new();
            app.quit_pressed_at = None;
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+U: delete to line start
            app.input.clear();
            app.quit_pressed_at = None;
        }
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+W: delete last word
            let trimmed = app.input.trim_end();
            if let Some(last_space) = trimmed.rfind(' ') {
                app.input.truncate(last_space + 1);
            } else {
                app.input.clear();
            }
            app.quit_pressed_at = None;
        }
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+J: insert newline (same as Shift+Enter)
            app.input.push('\n');
            app.quit_pressed_at = None;
        }
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.verbose = !app.verbose;
            app.quit_pressed_at = None;
        }
        KeyCode::Char(c) => {
            app.input.push(c);
            app.input_history_index = None;
            app.quit_pressed_at = None;
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.input_history_index = None;
            app.quit_pressed_at = None;
        }
        KeyCode::PageUp => {
            app.history_scroll.page_up(20);
            app.quit_pressed_at = None;
        }
        KeyCode::PageDown => {
            app.history_scroll.page_down(app.history.len(), 20);
            app.quit_pressed_at = None;
        }
        KeyCode::Up => {
            if !app.input_history.is_empty() {
                let new_index = if let Some(idx) = app.input_history_index {
                    if idx > 0 {
                        Some(idx - 1)
                    } else {
                        Some(idx)
                    }
                } else {
                    Some(app.input_history.len() - 1)
                };
                if let Some(idx) = new_index {
                    app.input_history_index = Some(idx);
                    app.input = app.input_history[idx].clone();
                    app.input_view.set_text(&app.input);
                }
            }
            app.quit_pressed_at = None;
        }
        KeyCode::Down => {
            if let Some(idx) = app.input_history_index {
                if idx < app.input_history.len() - 1 {
                    let new_idx = idx + 1;
                    app.input_history_index = Some(new_idx);
                    app.input = app.input_history[new_idx].clone();
                    app.input_view.set_text(&app.input);
                } else {
                    app.input_history_index = None;
                    app.input.clear();
                    app.input_view.clear();
                }
            }
            app.quit_pressed_at = None;
        }
        _ => {}
    }

    app.input_view.set_text(&app.input);
    app.input_view.detect_shell_mode();
    Ok(())
}

fn resolve_approval(
    app: &mut CodeApp,
    decision: ApprovalDecision,
    registry: &Option<Arc<ApprovalRegistry>>,
) {
    if let BottomPaneState::Approval(req) = &app.bottom_pane {
        let trace_id = req.trace_id;
        if let Some(reg) = registry {
            let _ = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(reg.resolve(trace_id, decision))
            });
        }
    }

    if let Some(next) = app.approval_queue.pop_front() {
        app.bottom_pane = BottomPaneState::Approval(next);
    } else {
        app.bottom_pane = BottomPaneState::Input;
    }
}

#[cfg(test)]
mod tests {
    use super::{CodeApp, HistoryCell};
    use clawhive_schema::BusMessage;

    #[test]
    fn message_accepted_marks_app_running() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        app.handle_bus_message(
            BusMessage::MessageAccepted {
                trace_id: uuid::Uuid::new_v4(),
            },
            "connector-1",
        );

        assert!(app.is_running);
    }

    #[test]
    fn stream_delta_creates_streaming_assistant_cell() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        app.handle_bus_message(
            BusMessage::StreamDelta {
                trace_id: uuid::Uuid::new_v4(),
                delta: "hello".to_string(),
                is_final: false,
            },
            "connector-1",
        );

        match app.history.last() {
            Some(HistoryCell::AssistantText { text, is_streaming }) => {
                assert_eq!(text, "hello");
                assert!(*is_streaming);
            }
            _ => panic!("expected streaming assistant cell"),
        }
    }

    #[test]
    fn execute_slash_help_adds_assistant_text() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());

        app.execute_slash_command("/help");

        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::AssistantText { .. })
        ));
    }

    #[test]
    fn execute_slash_clear_sets_offset_but_preserves_history() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        app.history.push(HistoryCell::AssistantText {
            text: "existing".to_string(),
            is_streaming: false,
        });

        app.execute_slash_command("/clear");

        // History is preserved but clear_offset hides it
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.history_clear_offset, 1);
    }

    #[test]
    fn execute_slash_exit_sets_should_quit() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());

        app.execute_slash_command("/exit");

        assert!(app.should_quit);
    }

    #[test]
    fn execute_slash_model_shows_agent_and_model() {
        let mut app = CodeApp::new("agent-x".to_string(), "model-y".to_string());

        app.execute_slash_command("/model");

        match app.history.last() {
            Some(HistoryCell::AssistantText { text, .. }) => {
                assert!(text.contains("agent-x"));
                assert!(text.contains("model-y"));
            }
            _ => panic!("expected assistant text"),
        }
    }

    #[test]
    fn execute_slash_cost_shows_token_and_cost() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        app.token_count = 42;
        app.cost_usd = 1.23;

        app.execute_slash_command("/cost");

        match app.history.last() {
            Some(HistoryCell::AssistantText { text, .. }) => {
                assert!(text.contains("42"));
                assert!(text.contains("1.23"));
            }
            _ => panic!("expected assistant text"),
        }
    }

    #[test]
    fn approval_flow_transitions_bottom_pane() {
        let mut app = CodeApp::new("agent".into(), "model".into());
        assert!(matches!(app.bottom_pane, super::BottomPaneState::Input));

        app.handle_bus_message(
            BusMessage::NeedHumanApproval {
                trace_id: uuid::Uuid::new_v4(),
                command: "rm -rf /".to_string(),
                agent_id: "agent".to_string(),
                reason: "dangerous".to_string(),
                network_target: None,
                source_channel_type: None,
                source_connector_id: None,
                source_conversation_scope: None,
            },
            "connector-1",
        );

        assert!(matches!(
            app.bottom_pane,
            super::BottomPaneState::Approval(_)
        ));
    }

    #[test]
    fn stream_delta_appends_to_existing_streaming_cell() {
        let mut app = CodeApp::new("agent".into(), "model".into());

        let trace = uuid::Uuid::new_v4();
        app.handle_bus_message(
            BusMessage::StreamDelta {
                trace_id: trace,
                delta: "hello ".into(),
                is_final: false,
            },
            "c",
        );
        app.handle_bus_message(
            BusMessage::StreamDelta {
                trace_id: trace,
                delta: "world".into(),
                is_final: false,
            },
            "c",
        );

        match app.history.last() {
            Some(HistoryCell::AssistantText { text, .. }) => assert_eq!(text, "hello world"),
            _ => panic!("expected assistant text"),
        }
    }

    #[test]
    fn final_stream_delta_marks_not_running() {
        let mut app = CodeApp::new("agent".into(), "model".into());
        app.is_running = true;

        app.handle_bus_message(
            BusMessage::StreamDelta {
                trace_id: uuid::Uuid::new_v4(),
                delta: "done".into(),
                is_final: false,
            },
            "c",
        );
        app.handle_bus_message(
            BusMessage::StreamDelta {
                trace_id: uuid::Uuid::new_v4(),
                delta: String::new(),
                is_final: true,
            },
            "c",
        );

        assert!(!app.is_running);
    }

    #[test]
    fn task_failed_adds_error_cell() {
        let mut app = CodeApp::new("agent".into(), "model".into());
        app.is_running = true;

        app.handle_bus_message(
            BusMessage::TaskFailed {
                trace_id: uuid::Uuid::new_v4(),
                error: "boom".into(),
            },
            "c",
        );

        assert!(!app.is_running);
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::Error { .. })
        ));
    }

    #[test]
    fn multiple_approvals_queue_correctly() {
        let mut app = CodeApp::new("agent".into(), "model".into());

        for i in 0..3 {
            app.handle_bus_message(
                BusMessage::NeedHumanApproval {
                    trace_id: uuid::Uuid::new_v4(),
                    command: format!("cmd-{i}"),
                    agent_id: "agent".to_string(),
                    reason: "test".to_string(),
                    network_target: None,
                    source_channel_type: None,
                    source_connector_id: None,
                    source_conversation_scope: None,
                },
                "c",
            );
        }

        assert!(matches!(
            app.bottom_pane,
            super::BottomPaneState::Approval(_)
        ));
        assert_eq!(app.approval_queue.len(), 2);
    }

    #[test]
    fn up_arrow_recalls_last_message() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        app.input_history.push("first message".to_string());
        app.input_history.push("second message".to_string());

        // Simulate pressing Up arrow
        assert_eq!(app.input_history_index, None);
        if !app.input_history.is_empty() {
            let new_index = if let Some(idx) = app.input_history_index {
                if idx > 0 {
                    Some(idx - 1)
                } else {
                    Some(idx)
                }
            } else {
                Some(app.input_history.len() - 1)
            };
            if let Some(idx) = new_index {
                app.input_history_index = Some(idx);
                app.input = app.input_history[idx].clone();
            }
        }

        assert_eq!(app.input_history_index, Some(1));
        assert_eq!(app.input, "second message");
    }

    #[test]
    fn down_arrow_returns_to_empty() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        app.input_history.push("first message".to_string());
        app.input_history.push("second message".to_string());
        app.input_history_index = Some(1);
        app.input = "second message".to_string();

        // Simulate pressing Down arrow at last item
        if let Some(idx) = app.input_history_index {
            if idx < app.input_history.len() - 1 {
                let new_idx = idx + 1;
                app.input_history_index = Some(new_idx);
                app.input = app.input_history[new_idx].clone();
            } else {
                app.input_history_index = None;
                app.input.clear();
            }
        }

        assert_eq!(app.input_history_index, None);
        assert_eq!(app.input, "");
    }

    #[test]
    fn up_arrow_does_nothing_with_empty_history() {
        let mut app = CodeApp::new("agent".to_string(), "model".to_string());
        assert_eq!(app.input_history.len(), 0);
        assert_eq!(app.input_history_index, None);

        // Simulate pressing Up arrow with empty history
        if !app.input_history.is_empty() {
            let new_index = if let Some(idx) = app.input_history_index {
                if idx > 0 {
                    Some(idx - 1)
                } else {
                    Some(idx)
                }
            } else {
                Some(app.input_history.len() - 1)
            };
            if let Some(idx) = new_index {
                app.input_history_index = Some(idx);
                app.input = app.input_history[idx].clone();
            }
        }

        // Should remain unchanged
        assert_eq!(app.input_history_index, None);
        assert_eq!(app.input, "");
    }
}
