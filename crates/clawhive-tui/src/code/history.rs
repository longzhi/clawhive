//! Conversation history primitives.

use std::time::Duration;

use chrono::{DateTime, Local};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use uuid::Uuid;

/// A single entry rendered in the history pane.
#[allow(dead_code)]
pub(crate) enum HistoryCell {
    UserMessage {
        text: String,
        timestamp: DateTime<Local>,
    },
    AssistantText {
        /// Accumulated streaming text (markdown source).
        text: String,
        is_streaming: bool,
    },
    Thinking {
        text: String,
        collapsed: bool,
    },
    ToolCall {
        tool_name: String,
        arguments: String,
        output: Option<ToolOutput>,
        duration: Option<Duration>,
        is_running: bool,
    },
    Error {
        trace_id: Uuid,
        message: String,
    },
}

/// Output payload shown for a tool call in history.
#[allow(dead_code)]
pub(crate) enum ToolOutput {
    /// Plain text lines (already truncated upstream).
    Text(Vec<String>),
    /// Unified diff for file edits.
    Diff {
        file_path: String,
        hunks: Vec<DiffHunk>,
    },
}

/// One unified-diff hunk section.
#[allow(dead_code)]
pub(crate) struct DiffHunk {
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

/// A line within a diff hunk.
#[allow(dead_code)]
pub(crate) enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
}

#[allow(dead_code)]
pub(crate) fn render_history_cell(
    cell: &HistoryCell,
    width: u16,
    verbose: bool,
) -> Vec<Line<'static>> {
    let width_usize = width.max(1) as usize;
    let dim = Style::default().add_modifier(Modifier::DIM);
    match cell {
        HistoryCell::UserMessage { text, .. } => {
            let wrapped = wrap_text(text, width_usize.saturating_sub(6).max(1));
            let mut out = Vec::with_capacity(wrapped.len().max(1));
            for (idx, part) in wrapped.iter().enumerate() {
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled("┃", Style::default().fg(Color::Cyan)),
                    Span::raw(" "),
                ];
                if idx == 0 {
                    spans.push(Span::styled(
                        "> ",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::raw(part.clone()));
                out.push(Line::from(spans));
            }
            out
        }
        HistoryCell::AssistantText { text, is_streaming } => {
            if *is_streaming && text.is_empty() {
                return vec![Line::from(vec![Span::raw("  "), Span::styled("...", dim)])];
            }
            super::markdown::render_markdown(text, width.saturating_sub(2).max(1))
                .into_iter()
                .map(|line| {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    Line::from(spans)
                })
                .collect()
        }
        HistoryCell::Thinking { text, collapsed } => {
            if *collapsed || !verbose {
                return vec![Line::from(Span::styled("  ╭─ Thinking ─╮", dim))];
            }

            let mut out = vec![Line::from(Span::styled("  ╭─ Thinking ─", dim))];
            for part in wrap_text(text, width_usize.saturating_sub(6).max(1)) {
                out.push(Line::from(Span::styled(format!("  │ {part}"), dim)));
            }
            out.push(Line::from(Span::styled("  ╰────────────", dim)));
            out
        }
        HistoryCell::ToolCall {
            tool_name,
            arguments,
            output,
            duration,
            is_running,
        } => {
            let mut out = Vec::new();
            let mut first_spans = vec![
                Span::raw("  "),
                Span::styled("⏺", Style::default().fg(Color::Cyan)),
                Span::raw(" "),
                Span::styled(
                    tool_name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];
            if !arguments.is_empty() {
                first_spans.push(Span::raw(" "));
                first_spans.push(Span::raw(arguments.clone()));
            }
            if let Some(duration) = duration {
                let left_text: String = first_spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let right_text = format!("{:.1}s", duration.as_secs_f64());
                let pad = width_usize
                    .saturating_sub(left_text.chars().count() + right_text.chars().count());
                if pad > 0 {
                    first_spans.push(Span::raw(" ".repeat(pad)));
                    first_spans.push(Span::styled(right_text, dim));
                }
            }
            out.push(Line::from(first_spans));

            if *is_running {
                out.push(Line::from(Span::styled("    ░░░░░░░░ running...", dim)));
            }

            if let Some(output) = output {
                match output {
                    ToolOutput::Text(lines) => {
                        for line in lines.iter().take(5) {
                            out.push(Line::from(Span::styled(format!("    ⎿ {line}"), dim)));
                        }
                        if lines.len() > 5 {
                            out.push(Line::from(Span::styled(
                                format!("    ⎿ ... {} more lines", lines.len() - 5),
                                dim,
                            )));
                        }
                    }
                    ToolOutput::Diff { file_path, hunks } => {
                        out.push(Line::from(Span::styled(format!("    {file_path}"), dim)));
                        for line in super::diff::render_diff(hunks, width.saturating_sub(4).max(1))
                        {
                            let mut spans = vec![Span::raw("    ")];
                            spans.extend(line.spans);
                            out.push(Line::from(spans));
                        }
                    }
                }
            }

            out
        }
        HistoryCell::Error { trace_id, message } => {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    "✗",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(message.clone(), Style::default().fg(Color::Red)),
            ];
            let trace = trace_id.to_string();
            spans.push(Span::raw(" "));
            spans.push(Span::styled(trace.chars().take(8).collect::<String>(), dim));
            vec![Line::from(spans)]
        }
    }
}

#[allow(dead_code)]
pub(crate) fn render_welcome_screen(width: u16, height: u16) -> Vec<Line<'static>> {
    let mut content = Vec::new();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let cyan = Style::default().fg(Color::Cyan);

    content.push(centered_line(width, Line::from(Span::raw("🐝"))));
    content.push(centered_line(
        width,
        Line::from(Span::styled("clawhive", bold)),
    ));
    content.push(Line::default());
    content.push(centered_line(
        width,
        Line::from(Span::styled("Your AI agent, ready to help.", dim)),
    ));
    content.push(Line::default());
    content.push(centered_line(
        width,
        Line::from(vec![
            Span::styled("/ commands", cyan),
            Span::raw("   "),
            Span::styled("@ files", cyan),
            Span::raw("   "),
            Span::styled("! shell", cyan),
            Span::raw("   "),
            Span::styled("? shortcuts", cyan),
        ]),
    ));

    let mut out = Vec::new();
    let top_pad = (height as usize).saturating_sub(content.len()) / 2;
    for _ in 0..top_pad {
        out.push(Line::default());
    }
    out.extend(content);
    out
}

#[allow(dead_code)]
pub(crate) fn render_history_pane(
    cells: &[HistoryCell],
    scroll: &super::scroll::ScrollState,
    area: Rect,
    buf: &mut Buffer,
    verbose: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines = if cells.is_empty() {
        render_welcome_screen(area.width, area.height)
    } else {
        let mut rendered = Vec::new();
        for (idx, cell) in cells.iter().enumerate() {
            rendered.extend(render_history_cell(cell, area.width, verbose));
            if idx + 1 < cells.len() {
                rendered.push(Line::default());
            }
        }
        rendered
    };

    let offset = scroll.offset().min(lines.len());
    lines = lines.into_iter().skip(offset).collect();

    for y in 0..area.height as usize {
        if let Some(line) = lines.get(y) {
            buf.set_line(area.x, area.y + y as u16, line, area.width);
        }
    }

    if offset > 0 {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let indicator = Line::from(Span::styled(format!("↑ {offset} more"), dim));
        let width = indicator.width() as u16;
        if width <= area.width {
            let x = area.x + area.width - width;
            buf.set_line(x, area.y, &indicator, width);
        }
    }
}

#[allow(dead_code)]
fn centered_line(width: u16, mut line: Line<'static>) -> Line<'static> {
    let line_width = line.width() as u16;
    let pad = width.saturating_sub(line_width) / 2;
    if pad > 0 {
        line.spans.insert(0, Span::raw(" ".repeat(pad as usize)));
    }
    line
}

#[allow(dead_code)]
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split_whitespace() {
            let next_len = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if !current.is_empty() && next_len > width {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if current.is_empty() {
            out.push(String::new());
        } else {
            out.push(current);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Local;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use uuid::Uuid;

    use super::{
        render_history_cell, render_history_pane, render_welcome_screen, HistoryCell, ToolOutput,
    };
    use crate::code::scroll::ScrollState;

    fn line_text(line: &ratatui::text::Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn buffer_text(buf: &Buffer, area: Rect) -> String {
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(area.x + x, area.y + y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn user_message_renders_with_prefix() {
        let cell = HistoryCell::UserMessage {
            text: "Fix the authentication bug in session.rs".into(),
            timestamp: Local::now(),
        };

        let lines = render_history_cell(&cell, 80, true);
        let text = line_text(&lines[0]);
        assert!(text.contains("┃ > "));
    }

    #[test]
    fn user_message_wraps_long_text() {
        let cell = HistoryCell::UserMessage {
            text: "word ".repeat(30),
            timestamp: Local::now(),
        };

        let lines = render_history_cell(&cell, 24, true);
        assert!(lines.len() > 1);
        assert!(line_text(&lines[0]).contains("┃ > "));
        assert!(line_text(&lines[1]).contains("┃   "));
    }

    #[test]
    fn assistant_text_renders_content() {
        let cell = HistoryCell::AssistantText {
            text: "hello world".into(),
            is_streaming: false,
        };

        let lines = render_history_cell(&cell, 80, true);
        assert_eq!(line_text(&lines[0]), "  hello world");
    }

    #[test]
    fn thinking_collapsed_renders_single_line() {
        let cell = HistoryCell::Thinking {
            text: "analyze steps".into(),
            collapsed: true,
        };

        let lines = render_history_cell(&cell, 80, true);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("Thinking"));
    }

    #[test]
    fn tool_call_renders_icon_and_tool_name() {
        let cell = HistoryCell::ToolCall {
            tool_name: "bash".into(),
            arguments: "ls".into(),
            output: None,
            duration: Some(Duration::from_millis(320)),
            is_running: false,
        };

        let lines = render_history_cell(&cell, 80, true);
        let first = line_text(&lines[0]);
        assert!(first.contains("⏺"));
        assert!(first.contains("bash"));
    }

    #[test]
    fn tool_call_text_output_is_truncated_at_five_lines() {
        let output = (1..=7).map(|n| format!("line {n}")).collect::<Vec<_>>();
        let cell = HistoryCell::ToolCall {
            tool_name: "read".into(),
            arguments: "file".into(),
            output: Some(ToolOutput::Text(output)),
            duration: None,
            is_running: false,
        };

        let lines = render_history_cell(&cell, 100, true)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line.contains("⎿ line 1")));
        assert!(lines.iter().any(|line| line.contains("... 2 more lines")));
    }

    #[test]
    fn error_renders_with_cross_icon() {
        let cell = HistoryCell::Error {
            trace_id: Uuid::nil(),
            message: "boom".into(),
        };

        let lines = render_history_cell(&cell, 80, true);
        assert!(line_text(&lines[0]).contains("✗ boom"));
    }

    #[test]
    fn welcome_screen_contains_clawhive_title() {
        let lines = render_welcome_screen(60, 12);
        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(texts.iter().any(|line| line.contains("clawhive")));
    }

    #[test]
    fn history_pane_empty_cells_show_welcome_screen() {
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        let scroll = ScrollState::new();

        render_history_pane(&[], &scroll, area, &mut buf, true);

        let text = buffer_text(&buf, area);
        assert!(text.contains("clawhive"));
    }
}
