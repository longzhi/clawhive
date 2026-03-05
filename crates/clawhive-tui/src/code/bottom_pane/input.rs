use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use tui_textarea::TextArea;

#[allow(dead_code)]
pub(crate) struct InputView {
    pub textarea: TextArea<'static>,
    pub is_shell_mode: bool,
}

#[allow(dead_code)]
impl InputView {
    pub(crate) fn new() -> Self {
        Self {
            textarea: build_textarea(Vec::new()),
            is_shell_mode: false,
        }
    }

    pub(crate) fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text().is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.textarea = build_textarea(Vec::new());
    }

    pub(crate) fn set_text(&mut self, text: &str) {
        self.textarea = build_textarea(text.split('\n').map(str::to_owned).collect());
    }

    pub(crate) fn desired_height(&self) -> u16 {
        let line_count = self.textarea.lines().len() as u16;
        line_count.clamp(2, 8)
    }

    pub(crate) fn detect_shell_mode(&mut self) {
        let first_line = self
            .textarea
            .lines()
            .first()
            .map(String::as_str)
            .unwrap_or("");
        self.is_shell_mode = first_line.starts_with('!');
    }
}

#[allow(dead_code)]
pub(crate) fn render_input_view(
    area: Rect,
    buf: &mut Buffer,
    input: &mut InputView,
    is_running: bool,
    agent_accent: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let border_style = Style::default().fg(agent_accent);
    for y in 0..area.height {
        if area.width >= 1 {
            buf[(area.x, area.y + y)].set_symbol(" ");
        }
        if area.width >= 2 {
            buf[(area.x + 1, area.y + y)].set_symbol(" ");
        }
        if area.width >= 3 {
            buf[(area.x + 2, area.y + y)]
                .set_symbol("┃")
                .set_style(border_style);
        }
        if area.width >= 4 {
            buf[(area.x + 3, area.y + y)].set_symbol(" ");
        }
        if area.width >= 6 {
            buf[(area.x + 4, area.y + y)].set_symbol(" ");
            buf[(area.x + 5, area.y + y)].set_symbol(" ");
        }
    }

    if area.width >= 5 {
        buf[(area.x + 4, area.y)]
            .set_symbol("›")
            .set_style(Style::default().add_modifier(Modifier::BOLD));
    }

    let text_area = Rect::new(
        area.x.saturating_add(6),
        area.y,
        area.width.saturating_sub(6),
        area.height,
    );
    if text_area.width == 0 || text_area.height == 0 {
        return;
    }

    if is_running {
        let shimmer = super::super::shimmer::shimmer_line(text_area.width);
        buf.set_line(text_area.x, text_area.y, &shimmer, text_area.width);
    } else {
        input.textarea.render(text_area, buf);
    }
}

fn build_textarea(lines: Vec<String>) -> TextArea<'static> {
    let mut textarea = TextArea::new(lines);
    textarea.set_placeholder_text("Ask clawhive anything...");
    textarea.set_placeholder_style(Style::default().add_modifier(Modifier::DIM));
    textarea.set_block(ratatui::widgets::Block::default());
    textarea.set_cursor_line_style(Style::default());
    textarea
}

#[cfg(test)]
mod tests {
    use super::InputView;

    #[test]
    fn input_view_starts_empty() {
        let input = InputView::new();
        assert!(input.is_empty());
        assert_eq!(input.text(), "");
    }

    #[test]
    fn desired_height_is_two_when_empty() {
        let input = InputView::new();
        assert_eq!(input.desired_height(), 2);
    }

    #[test]
    fn desired_height_tracks_line_count_and_caps_at_eight() {
        let mut input = InputView::new();
        input.set_text("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine");
        assert_eq!(input.desired_height(), 8);

        input.set_text("one\ntwo\nthree");
        assert_eq!(input.desired_height(), 3);
    }

    #[test]
    fn shell_mode_detected_when_input_starts_with_bang() {
        let mut input = InputView::new();
        input.set_text("!ls -la");
        input.detect_shell_mode();
        assert!(input.is_shell_mode);
    }

    #[test]
    fn clear_empties_text() {
        let mut input = InputView::new();
        input.set_text("hello");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.text(), "");
    }
}
