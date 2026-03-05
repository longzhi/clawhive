use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[allow(dead_code)]
pub(crate) fn filter_paths(paths: &[String], query: &str) -> Vec<String> {
    if query.is_empty() {
        return paths.iter().take(8).cloned().collect();
    }

    let needle = query.to_lowercase();
    paths
        .iter()
        .filter(|path| path.to_lowercase().contains(&needle))
        .take(8)
        .cloned()
        .collect()
}

#[allow(dead_code)]
pub(crate) fn render_file_search_picker(
    area: Rect,
    buf: &mut Buffer,
    filtered: &[String],
    selected: usize,
) {
    if area.width == 0 || area.height < 2 {
        return;
    }

    let dim = Style::default().add_modifier(Modifier::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    draw_border(area, buf, dim);

    let visible = filtered
        .len()
        .min(8)
        .min(area.height.saturating_sub(2) as usize);
    let selected_idx = selected.min(visible.saturating_sub(1));
    for (row, path) in filtered.iter().take(visible).enumerate() {
        let y = area.y + 1 + row as u16;
        let prefix = if row == selected_idx { "› " } else { "  " };
        let line = if row == selected_idx {
            Line::from(vec![
                Span::styled(prefix, bold),
                Span::styled(path.clone(), bold),
            ])
        } else {
            Line::from(vec![Span::raw(prefix), Span::raw(path.clone())])
        };
        buf.set_line(area.x + 1, y, &line, area.width.saturating_sub(2));
    }
}

#[allow(dead_code)]
pub(crate) fn file_picker_height(count: usize) -> u16 {
    count.min(8) as u16 + 2
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

    use super::{file_picker_height, filter_paths, render_file_search_picker};

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
    fn filter_paths_matches_query() {
        let paths = vec![
            "src/auth/session.rs".to_string(),
            "src/auth/token.rs".to_string(),
            "src/ui/mod.rs".to_string(),
        ];

        let filtered = filter_paths(&paths, "auth");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|path| path.contains("auth")));
    }

    #[test]
    fn filter_paths_caps_at_eight_results() {
        let paths: Vec<String> = (0..12).map(|i| format!("src/file_{i}.rs")).collect();

        let filtered = filter_paths(&paths, "");
        assert_eq!(filtered.len(), 8);
    }

    #[test]
    fn render_file_search_picker_shows_selected_with_marker() {
        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        let filtered = vec![
            "src/auth/session.rs".to_string(),
            "src/auth/token.rs".to_string(),
        ];

        render_file_search_picker(area, &mut buf, &filtered, 0);

        let text = buffer_text(&buf, area);
        assert!(text.contains("› src/auth/session.rs"));
    }

    #[test]
    fn file_picker_height_includes_borders() {
        assert_eq!(file_picker_height(10), 10);
    }
}
