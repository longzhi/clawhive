use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[allow(dead_code)]
pub(crate) fn render_shortcut_overlay(area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let title_style = Style::default().add_modifier(Modifier::BOLD);
    let dim_style = Style::default().add_modifier(Modifier::DIM);
    let key_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);

    if area.height >= 1 {
        buf.set_line(
            area.x,
            area.y,
            &Line::from(Span::styled("Keyboard Shortcuts", title_style)),
            area.width,
        );
    }

    if area.height >= 2 {
        let sep = "─".repeat(area.width as usize);
        buf.set_line(
            area.x,
            area.y + 1,
            &Line::from(Span::styled(sep, dim_style)),
            area.width,
        );
    }

    let rows = [
        (("/", "slash commands"), ("@", "file paths")),
        (("!", "shell command"), ("?", "this help")),
        (("shift+enter", "newline"), ("tab", "queue message")),
        (("ctrl+g", "external editor"), ("ctrl+v", "paste image")),
        (("esc", "interrupt / back"), ("ctrl+c", "quit")),
        (("esc esc", "rewind checkpoint"), ("ctrl+l", "clear screen")),
        (("ctrl+o", "toggle verbose"), ("", "")),
    ];

    let col_width = area.width / 2;
    for (idx, (left, right)) in rows.iter().enumerate() {
        let y = area.y + 2 + idx as u16;
        if y >= area.y + area.height {
            break;
        }
        render_pair(area.x, y, col_width, *left, buf, key_style, dim_style);
        if col_width < area.width {
            render_pair(
                area.x + col_width,
                y,
                area.width - col_width,
                *right,
                buf,
                key_style,
                dim_style,
            );
        }
    }

    let dismiss_y = area.y + 10;
    if dismiss_y < area.y + area.height {
        let hint = "press any key to dismiss";
        let hint_width = hint.chars().count() as u16;
        let x = if area.width > hint_width {
            area.x + (area.width - hint_width) / 2
        } else {
            area.x
        };
        buf.set_line(
            x,
            dismiss_y,
            &Line::from(Span::styled(hint, dim_style)),
            area.width.saturating_sub(x.saturating_sub(area.x)),
        );
    }
}

#[allow(dead_code)]
pub(crate) fn shortcut_overlay_height() -> u16 {
    11
}

#[allow(dead_code)]
fn render_pair(
    x: u16,
    y: u16,
    width: u16,
    item: (&str, &str),
    buf: &mut Buffer,
    key_style: Style,
    desc_style: Style,
) {
    if width == 0 {
        return;
    }

    let key_width = 16_u16.min(width);
    let key = format!("{:<width$}", item.0, width = key_width as usize);
    let line = Line::from(vec![
        Span::styled(key, key_style),
        Span::styled(item.1.to_string(), desc_style),
    ]);
    buf.set_line(x, y, &line, width);
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::{render_shortcut_overlay, shortcut_overlay_height};

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
    fn render_shortcut_overlay_contains_title() {
        let area = Rect::new(0, 0, 72, 11);
        let mut buf = Buffer::empty(area);

        render_shortcut_overlay(area, &mut buf);

        let text = buffer_text(&buf, area);
        assert!(text.contains("Keyboard Shortcuts"));
    }

    #[test]
    fn render_shortcut_overlay_contains_shortcut_descriptions() {
        let area = Rect::new(0, 0, 72, 11);
        let mut buf = Buffer::empty(area);

        render_shortcut_overlay(area, &mut buf);

        let text = buffer_text(&buf, area);
        assert!(text.contains("slash commands"));
    }

    #[test]
    fn shortcut_overlay_height_is_fixed() {
        assert_eq!(shortcut_overlay_height(), 11);
    }
}
