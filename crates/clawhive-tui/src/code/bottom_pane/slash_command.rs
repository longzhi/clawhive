use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[allow(dead_code)]
pub(crate) struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

#[allow(dead_code)]
pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/compact",
        description: "Compress conversation history",
    },
    SlashCommand {
        name: "/context",
        description: "Show context window usage",
    },
    SlashCommand {
        name: "/cost",
        description: "Show token usage and cost",
    },
    SlashCommand {
        name: "/diff",
        description: "Show files changed this session",
    },
    SlashCommand {
        name: "/clear",
        description: "Clear screen (keep history)",
    },
    SlashCommand {
        name: "/model",
        description: "Show current model info",
    },
    SlashCommand {
        name: "/help",
        description: "Show help and commands",
    },
    SlashCommand {
        name: "/exit",
        description: "Exit the TUI",
    },
];

#[allow(dead_code)]
pub(crate) fn filter_commands(query: &str) -> Vec<&'static SlashCommand> {
    if query.is_empty() {
        return SLASH_COMMANDS.iter().collect();
    }

    let needle = query.to_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|cmd| cmd.name.to_lowercase().contains(&needle))
        .collect()
}

#[allow(dead_code)]
pub(crate) fn render_slash_command_picker(
    area: Rect,
    buf: &mut Buffer,
    query: &str,
    selected: usize,
) {
    if area.width == 0 || area.height < 2 {
        return;
    }

    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    draw_border(area, buf, dim);

    let filtered = filter_commands(query);
    let visible = filtered
        .len()
        .min(8)
        .min(area.height.saturating_sub(2) as usize);
    let selected_idx = selected.min(visible.saturating_sub(1));

    for (row, item) in filtered.iter().take(visible).enumerate() {
        let y = area.y + 1 + row as u16;
        let item = *item;

        let prefix = if row == selected_idx { "› " } else { "  " };
        let name = format!("{:<12}", item.name);
        let line = if row == selected_idx {
            Line::from(vec![
                Span::styled(prefix, bold),
                Span::styled(name, bold),
                Span::styled(item.description.to_string(), dim),
            ])
        } else {
            Line::from(vec![
                Span::raw(prefix),
                Span::raw(name),
                Span::styled(item.description.to_string(), dim),
            ])
        };
        buf.set_line(area.x + 1, y, &line, area.width.saturating_sub(2));
    }
}

#[allow(dead_code)]
pub(crate) fn slash_picker_height(query: &str) -> u16 {
    filter_commands(query).len().min(8) as u16 + 2
}

#[allow(dead_code)]
fn draw_border(area: Rect, buf: &mut Buffer, style: Style) {
    if area.width < 2 {
        return;
    }

    buf[(area.x, area.y)].set_symbol("┌").set_style(style);
    buf[(area.x + area.width - 1, area.y)]
        .set_symbol("┐")
        .set_style(style);
    for x in (area.x + 1)..(area.x + area.width - 1) {
        buf[(x, area.y)].set_symbol("─").set_style(style);
    }

    let bottom = area.y + area.height - 1;
    buf[(area.x, bottom)].set_symbol("└").set_style(style);
    buf[(area.x + area.width - 1, bottom)]
        .set_symbol("┘")
        .set_style(style);
    for x in (area.x + 1)..(area.x + area.width - 1) {
        buf[(x, bottom)].set_symbol("─").set_style(style);
    }

    for y in (area.y + 1)..bottom {
        buf[(area.x, y)].set_symbol("│").set_style(style);
        buf[(area.x + area.width - 1, y)]
            .set_symbol("│")
            .set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::{
        filter_commands, render_slash_command_picker, slash_picker_height, SLASH_COMMANDS,
    };

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
    fn filter_commands_empty_returns_all() {
        let filtered = filter_commands("");
        assert_eq!(filtered.len(), SLASH_COMMANDS.len());
    }

    #[test]
    fn filter_commands_co_matches_expected_names() {
        let filtered = filter_commands("co");
        let names: Vec<&str> = filtered.iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["/compact", "/context", "/cost"]);
    }

    #[test]
    fn render_slash_command_picker_shows_selected_with_marker() {
        let area = Rect::new(0, 0, 60, 6);
        let mut buf = Buffer::empty(area);

        render_slash_command_picker(area, &mut buf, "co", 1);

        let text = buffer_text(&buf, area);
        assert!(text.contains("› /context"));
    }

    #[test]
    fn slash_picker_height_includes_borders() {
        assert_eq!(slash_picker_height("co"), 5);
    }
}
