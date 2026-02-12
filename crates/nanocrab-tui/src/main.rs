use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};

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
}

impl App {
    fn new() -> Self {
        Self {
            events: vec![format!(
                "[{}] TUI started",
                chrono::Local::now().format("%H:%M:%S")
            )],
            sessions: vec!["No active sessions".into()],
            agent_runs: vec!["No running agents".into()],
            logs: vec!["Waiting for trace events...".into()],
            focus: Panel::BusEvents,
            scroll_offset: [0; 4],
            should_quit: false,
        }
    }

    fn on_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') => self.should_quit = true,
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
                    Panel::BusEvents => self.events.len(),
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

    /// Push a bus event into the panel (used when real EventBus is wired).
    #[allow(dead_code)]
    fn push_event(&mut self, event: String) {
        self.events.push(event);
        if self.events.len() > MAX_ITEMS {
            self.events.remove(0);
        }
    }

    /// Push a log/trace entry into the panel (used when real EventBus is wired).
    #[allow(dead_code)]
    fn push_log(&mut self, log: String) {
        self.logs.push(log);
        if self.logs.len() > MAX_ITEMS {
            self.logs.remove(0);
        }
    }
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let run_result = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    run_result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|frame| ui(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
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

    render_list_panel(
        frame,
        top_cols[0],
        " Bus Events ",
        &app.events,
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

    let status = Paragraph::new(Line::from(vec![
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
        Span::styled(
            "| nanocrab TUI v0.2.0 ",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
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
