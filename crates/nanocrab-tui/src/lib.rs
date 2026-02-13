use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nanocrab_bus::{EventBus, Topic};
use nanocrab_schema::BusMessage;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};
use tokio::sync::mpsc;

const MAX_ITEMS: usize = 200;

#[derive(Clone, Copy, PartialEq)]
enum Panel {
    BusEvents,
    Sessions,
    AgentRuns,
    Logs,
}

impl Panel {
    fn next(self) -> Self {
        match self {
            Panel::BusEvents => Panel::Sessions,
            Panel::Sessions => Panel::AgentRuns,
            Panel::AgentRuns => Panel::Logs,
            Panel::Logs => Panel::BusEvents,
        }
    }
}

struct App {
    events: Vec<String>,
    sessions: Vec<String>,
    agent_runs: Vec<String>,
    logs: Vec<String>,
    focus: Panel,
    scroll_offset: [usize; 4],
    should_quit: bool,
    trace_filter: Option<String>,
    filter_input: String,
    filter_mode: bool,
}

impl App {
    fn new() -> Self {
        Self {
            events: vec![format!(
                "[{}] TUI started — listening on EventBus",
                chrono::Local::now().format("%H:%M:%S")
            )],
            sessions: vec!["No active sessions".into()],
            agent_runs: vec!["No running agents".into()],
            logs: vec!["Waiting for bus events...".into()],
            focus: Panel::BusEvents,
            scroll_offset: [0; 4],
            should_quit: false,
            trace_filter: None,
            filter_input: String::new(),
            filter_mode: false,
        }
    }

    fn on_key(&mut self, key: KeyCode) {
        if self.filter_mode {
            match key {
                KeyCode::Enter => {
                    self.trace_filter = if self.filter_input.is_empty() {
                        None
                    } else {
                        Some(self.filter_input.clone())
                    };
                    self.filter_mode = false;
                }
                KeyCode::Esc => {
                    self.filter_mode = false;
                    self.filter_input.clear();
                }
                KeyCode::Backspace => {
                    self.filter_input.pop();
                }
                KeyCode::Char(c) => {
                    self.filter_input.push(c);
                }
                _ => {}
            }
            return;
        }

        match key {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('/') => {
                self.filter_mode = true;
                self.filter_input.clear();
            }
            KeyCode::Tab => {
                self.focus = self.focus.next();
            }
            KeyCode::Up => {
                let idx = self.focus as usize;
                if self.scroll_offset[idx] > 0 {
                    self.scroll_offset[idx] -= 1;
                }
            }
            KeyCode::Down => {
                let idx = self.focus as usize;
                let max = match self.focus {
                    Panel::BusEvents => self.filtered_events().len(),
                    Panel::Sessions => self.sessions.len(),
                    Panel::AgentRuns => self.agent_runs.len(),
                    Panel::Logs => self.logs.len(),
                };
                if self.scroll_offset[idx] < max.saturating_sub(1) {
                    self.scroll_offset[idx] += 1;
                }
            }
            _ => {}
        }
    }

    fn filtered_events(&self) -> Vec<&String> {
        match &self.trace_filter {
            Some(filter) => self.events.iter().filter(|e| e.contains(filter)).collect(),
            None => self.events.iter().collect(),
        }
    }

    fn push_event(&mut self, line: String) {
        self.events.push(line);
        if self.events.len() > MAX_ITEMS {
            self.events.remove(0);
        }
    }

    fn push_session(&mut self, line: String) {
        if self.sessions.first().map(|s| s.as_str()) == Some("No active sessions") {
            self.sessions.clear();
        }
        self.sessions.push(line);
        if self.sessions.len() > MAX_ITEMS {
            self.sessions.remove(0);
        }
    }

    fn push_agent_run(&mut self, line: String) {
        if self.agent_runs.first().map(|s| s.as_str()) == Some("No running agents") {
            self.agent_runs.clear();
        }
        self.agent_runs.push(line);
        if self.agent_runs.len() > MAX_ITEMS {
            self.agent_runs.remove(0);
        }
    }

    fn push_log(&mut self, line: String) {
        if self.logs.first().map(|s| s.as_str()) == Some("Waiting for bus events...") {
            self.logs.clear();
        }
        self.logs.push(line);
        if self.logs.len() > MAX_ITEMS {
            self.logs.remove(0);
        }
    }

    fn handle_bus_message(&mut self, msg: BusMessage) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        match msg {
            BusMessage::HandleIncomingMessage {
                ref inbound,
                ref resolved_agent_id,
            } => {
                self.push_event(format!(
                    "[{ts}] HandleIncoming trace={} agent={resolved_agent_id}",
                    &inbound.trace_id.to_string()[..8]
                ));
                self.push_session(format!(
                    "[{ts}] {} → {} ({})",
                    inbound.user_scope, inbound.conversation_scope, inbound.channel_type
                ));
            }
            BusMessage::MessageAccepted { trace_id } => {
                self.push_event(format!(
                    "[{ts}] MessageAccepted trace={}",
                    &trace_id.to_string()[..8]
                ));
                self.push_agent_run(format!(
                    "[{ts}] trace={} running",
                    &trace_id.to_string()[..8]
                ));
            }
            BusMessage::ReplyReady { ref outbound } => {
                let preview: String = outbound.text.chars().take(60).collect();
                self.push_event(format!(
                    "[{ts}] ReplyReady trace={}",
                    &outbound.trace_id.to_string()[..8]
                ));
                self.push_agent_run(format!(
                    "[{ts}] trace={} completed",
                    &outbound.trace_id.to_string()[..8]
                ));
                self.push_log(format!("[{ts}] Reply: {preview}"));
            }
            BusMessage::TaskFailed {
                trace_id,
                ref error,
            } => {
                self.push_event(format!(
                    "[{ts}] TaskFailed trace={}",
                    &trace_id.to_string()[..8]
                ));
                self.push_agent_run(format!(
                    "[{ts}] trace={} failed",
                    &trace_id.to_string()[..8]
                ));
                self.push_log(format!("[{ts}] ERROR: {error}"));
            }
            BusMessage::CancelTask { trace_id } => {
                self.push_event(format!(
                    "[{ts}] CancelTask trace={}",
                    &trace_id.to_string()[..8]
                ));
                self.push_agent_run(format!(
                    "[{ts}] Cancelled trace={}",
                    &trace_id.to_string()[..8]
                ));
            }
            BusMessage::RunScheduledConsolidation => {
                self.push_event(format!("[{ts}] RunScheduledConsolidation"));
                self.push_agent_run(format!("[{ts}] Consolidation triggered"));
            }
            BusMessage::MemoryWriteRequested {
                ref session_key,
                ref speaker,
                ref text,
                importance,
            } => {
                let preview: String = text.chars().take(40).collect();
                self.push_event(format!(
                    "[{ts}] MemoryWrite session={session_key} speaker={speaker}"
                ));
                self.push_log(format!("[{ts}] Mem[{importance:.1}]: {preview}"));
            }
            BusMessage::NeedHumanApproval {
                trace_id,
                ref reason,
            } => {
                self.push_event(format!(
                    "[{ts}] NeedHumanApproval trace={}",
                    &trace_id.to_string()[..8]
                ));
                self.push_log(format!("[{ts}] APPROVAL: {reason}"));
            }
            BusMessage::MemoryReadRequested {
                ref session_key,
                ref query,
            } => {
                let preview: String = query.chars().take(40).collect();
                self.push_event(format!("[{ts}] MemoryRead session={session_key}"));
                self.push_log(format!("[{ts}] Query: {preview}"));
            }
            BusMessage::ConsolidationCompleted {
                concepts_created,
                concepts_updated,
                episodes_processed,
            } => {
                self.push_event(format!("[{ts}] ConsolidationCompleted"));
                self.push_agent_run(format!(
                    "[{ts}] Consolidation done: +{concepts_created} concepts, ~{concepts_updated} updated, {episodes_processed} eps"
                ));
            }
            BusMessage::StreamDelta {
                trace_id,
                ref delta,
                is_final,
            } => {
                if is_final {
                    self.push_event(format!(
                        "[{ts}] StreamComplete trace={}",
                        &trace_id.to_string()[..8]
                    ));
                } else if !delta.is_empty() {
                    self.push_log(format!(
                        "[{ts}] Stream[{}]: {}",
                        &trace_id.to_string()[..8],
                        delta.chars().take(60).collect::<String>()
                    ));
                }
            }
        }
    }
}

pub struct BusReceivers {
    handle_incoming: mpsc::Receiver<BusMessage>,
    cancel_task: mpsc::Receiver<BusMessage>,
    consolidation: mpsc::Receiver<BusMessage>,
    message_accepted: mpsc::Receiver<BusMessage>,
    reply_ready: mpsc::Receiver<BusMessage>,
    task_failed: mpsc::Receiver<BusMessage>,
    memory_write: mpsc::Receiver<BusMessage>,
    need_human_approval: mpsc::Receiver<BusMessage>,
    memory_read: mpsc::Receiver<BusMessage>,
    consolidation_completed: mpsc::Receiver<BusMessage>,
    stream_delta: mpsc::Receiver<BusMessage>,
}

impl BusReceivers {
    fn drain_all(&mut self, app: &mut App) {
        while let Ok(msg) = self.handle_incoming.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.cancel_task.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.consolidation.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.message_accepted.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.reply_ready.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.task_failed.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.memory_write.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.need_human_approval.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.memory_read.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.consolidation_completed.try_recv() {
            app.handle_bus_message(msg);
        }
        while let Ok(msg) = self.stream_delta.try_recv() {
            app.handle_bus_message(msg);
        }
    }
}

pub async fn subscribe_all(bus: &EventBus) -> BusReceivers {
    BusReceivers {
        handle_incoming: bus.subscribe(Topic::HandleIncomingMessage).await,
        cancel_task: bus.subscribe(Topic::CancelTask).await,
        consolidation: bus.subscribe(Topic::RunScheduledConsolidation).await,
        message_accepted: bus.subscribe(Topic::MessageAccepted).await,
        reply_ready: bus.subscribe(Topic::ReplyReady).await,
        task_failed: bus.subscribe(Topic::TaskFailed).await,
        memory_write: bus.subscribe(Topic::MemoryWriteRequested).await,
        need_human_approval: bus.subscribe(Topic::NeedHumanApproval).await,
        memory_read: bus.subscribe(Topic::MemoryReadRequested).await,
        consolidation_completed: bus.subscribe(Topic::ConsolidationCompleted).await,
        stream_delta: bus.subscribe(Topic::StreamDelta).await,
    }
}

pub async fn run_tui(bus: &EventBus) -> Result<()> {
    let receivers = subscribe_all(bus).await;
    run_tui_from_receivers(receivers).await
}

pub async fn run_tui_from_receivers(receivers: BusReceivers) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let run_result = run_app(&mut terminal, &mut app, receivers);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    run_result
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    mut receivers: BusReceivers,
) -> Result<()> {
    loop {
        receivers.drain_all(app);

        terminal.draw(|frame| ui(frame, app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn ui(frame: &mut Frame, app: &App) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_layout[0]);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[0]);

    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[1]);

    let filtered: Vec<String> = app
        .filtered_events()
        .iter()
        .map(|s| s.to_string())
        .collect();

    render_list_panel(
        frame,
        top_cols[0],
        " Bus Events ",
        &filtered,
        app.scroll_offset[0],
        app.focus == Panel::BusEvents,
        Color::Cyan,
    );

    render_list_panel(
        frame,
        top_cols[1],
        " Sessions ",
        &app.sessions,
        app.scroll_offset[1],
        app.focus == Panel::Sessions,
        Color::Yellow,
    );

    render_list_panel(
        frame,
        bottom_cols[0],
        " Agent Runs ",
        &app.agent_runs,
        app.scroll_offset[2],
        app.focus == Panel::AgentRuns,
        Color::Green,
    );

    render_list_panel(
        frame,
        bottom_cols[1],
        " Logs / Trace ",
        &app.logs,
        app.scroll_offset[3],
        app.focus == Panel::Logs,
        Color::Magenta,
    );

    let mut spans = vec![
        Span::styled(
            " [q]",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[Tab]",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" focus ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "[↑↓]",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" scroll ", Style::default().fg(Color::DarkGray)),
    ];

    let filter_span = if let Some(ref filter) = app.trace_filter {
        Span::styled(
            format!("[/] filter: {filter} "),
            Style::default().fg(Color::Yellow),
        )
    } else if app.filter_mode {
        Span::styled(
            format!("[/] typing: {}_ ", app.filter_input),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::styled("[/] filter ", Style::default().fg(Color::DarkGray))
    };
    spans.push(filter_span);
    spans.push(Span::styled(
        "| nanocrab TUI v0.3.0 ",
        Style::default().fg(Color::DarkGray),
    ));

    let status = Paragraph::new(Line::from(spans));
    frame.render_widget(status, main_layout[1]);
}

fn render_list_panel(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    title: &str,
    items: &[String],
    scroll_offset: usize,
    focused: bool,
    color: Color,
) {
    let border_style = if focused {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let visible: Vec<ListItem> = items
        .iter()
        .skip(scroll_offset)
        .map(|item| {
            let style = if focused {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(Span::styled(item.as_str(), style)))
        })
        .collect();

    let list = List::new(visible).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style),
    );

    frame.render_widget(list, area);
}

#[cfg(test)]
mod tests {
    use nanocrab_schema::{BusMessage, InboundMessage, OutboundMessage};

    use super::App;

    #[test]
    fn message_accepted_populates_agent_runs_panel() {
        let mut app = App::new();
        let trace_id = uuid::Uuid::new_v4();

        app.handle_bus_message(BusMessage::MessageAccepted { trace_id });

        assert_ne!(app.agent_runs, vec!["No running agents".to_string()]);
        assert!(app
            .agent_runs
            .iter()
            .any(|line| line.contains("trace=") && line.contains("running")));
    }

    #[test]
    fn reply_ready_populates_agent_runs_panel() {
        let mut app = App::new();
        let outbound = OutboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            text: "done".into(),
            at: chrono::Utc::now(),
        };

        app.handle_bus_message(BusMessage::ReplyReady { outbound });

        assert_ne!(app.agent_runs, vec!["No running agents".to_string()]);
        assert!(app
            .agent_runs
            .iter()
            .any(|line| line.contains("trace=") && line.contains("completed")));
    }

    #[test]
    fn handle_incoming_populates_sessions_panel() {
        let mut app = App::new();
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "hi".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        app.handle_bus_message(BusMessage::HandleIncomingMessage {
            inbound,
            resolved_agent_id: "nanocrab-main".into(),
        });

        assert_ne!(app.sessions, vec!["No active sessions".to_string()]);
    }
}
