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

struct App {
    events: Vec<String>,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            should_quit: false,
        }
    }

    fn on_key(&mut self, key: KeyCode) {
        if let KeyCode::Char('q') = key {
            self.should_quit = true;
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

    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(main_layout[0]);

    let events = if app.events.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No events yet...",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        app.events
            .iter()
            .rev()
            .map(|item| ListItem::new(Line::from(item.as_str())))
            .collect()
    };

    let events_list = List::new(events).block(
        Block::default()
            .title(" Bus Events ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(events_list, panels[0]);

    let sessions = Paragraph::new("No active sessions").block(
        Block::default()
            .title(" Sessions ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(sessions, panels[1]);

    let status = Paragraph::new(Line::from(vec![
        Span::styled(" Press ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "'q'",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " to quit | nanocrab TUI v0.1.0 ",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    frame.render_widget(status, main_layout[1]);
}
