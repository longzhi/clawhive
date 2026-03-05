#![allow(dead_code)]

use std::mem;
use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(two_face::syntax::extra_newlines)
}

fn highlight_theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let ts = two_face::theme::extra();
        ts.get(two_face::theme::EmbeddedThemeName::Base16OceanDark)
            .clone()
    })
}

struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    current_style: Style,
    width: u16,
    in_code_block: bool,
    code_lang: String,
    style_stack: Vec<Style>,
    pending_paragraph_blank: bool,
    list_depth: usize,
    in_list_item: bool,
    current_item_prefix: String,
    current_item_continuation_prefix: String,
    link_target: Option<String>,
    link_text: String,
    code_highlighter: Option<HighlightLines<'static>>,
}

impl MarkdownRenderer {
    fn new(width: u16) -> Self {
        Self {
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_style: Style::default(),
            width,
            in_code_block: false,
            code_lang: String::new(),
            style_stack: vec![Style::default()],
            pending_paragraph_blank: false,
            list_depth: 0,
            in_list_item: false,
            current_item_prefix: String::new(),
            current_item_continuation_prefix: String::new(),
            link_target: None,
            link_text: String::new(),
            code_highlighter: None,
        }
    }

    fn max_width(&self) -> usize {
        self.width.max(1) as usize
    }

    fn ensure_block_spacing(&mut self) {
        if self.pending_paragraph_blank && !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.pending_paragraph_blank = false;
    }

    fn push_span(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        self.current_spans.push(Span::styled(text, style));
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        if self.in_code_block {
            for part in text.split('\n') {
                self.push_code_block_line(part);
            }
            return;
        }

        if self.link_target.is_some() {
            self.link_text.push_str(text);
            return;
        }

        for (idx, part) in text.split('\n').enumerate() {
            if idx > 0 {
                self.flush_current_as_wrapped();
            }
            self.push_span(part.to_owned(), self.current_style);
        }
    }

    fn push_code_block_line(&mut self, part: &str) {
        if let Some(highlighter) = self.code_highlighter.as_mut() {
            if let Ok(ranges) = highlighter.highlight_line(part, syntax_set()) {
                let mut spans = vec![Span::styled(
                    "│ ",
                    Style::default().add_modifier(Modifier::DIM),
                )];
                for (style, token) in ranges {
                    if token.is_empty() {
                        continue;
                    }
                    spans.push(Span::styled(
                        token.to_owned(),
                        Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        )),
                    ));
                }
                self.lines.push(Line::from(spans));
                return;
            }
        }

        self.lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(part.to_owned(), Style::default().fg(Color::DarkGray)),
        ]));
    }

    fn code_syntax(&self) -> Option<&'static SyntaxReference> {
        let lang = self
            .code_lang
            .trim()
            .split(|ch: char| ch.is_whitespace() || ch == ',')
            .next()
            .unwrap_or_default();
        if lang.is_empty() {
            return None;
        }
        syntax_set()
            .find_syntax_by_token(lang)
            .or_else(|| syntax_set().find_syntax_by_extension(lang))
    }

    fn build_code_highlighter(&self) -> Option<HighlightLines<'static>> {
        let syntax = self.code_syntax()?;
        Some(HighlightLines::new(syntax, highlight_theme()))
    }

    fn flush_current_as_wrapped(&mut self) {
        if self.current_spans.is_empty() {
            return;
        }
        let spans = mem::take(&mut self.current_spans);
        let first_prefix = if self.in_list_item {
            self.current_item_prefix.as_str()
        } else {
            ""
        };
        let next_prefix = if self.in_list_item {
            self.current_item_continuation_prefix.as_str()
        } else {
            ""
        };
        let mut wrapped = wrap_spans(spans, self.max_width(), first_prefix, next_prefix);
        self.lines.append(&mut wrapped);
    }

    fn push_code_block_header(&mut self) {
        let header = if self.code_lang.trim().is_empty() {
            "╭─".to_owned()
        } else {
            format!("╭─ {} ─", self.code_lang.trim())
        };
        self.lines.push(Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    fn push_code_block_footer(&mut self) {
        self.lines.push(Line::from(Span::styled(
            "╰─",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    fn finalize(mut self) -> Vec<Line<'static>> {
        self.flush_current_as_wrapped();
        self.lines
    }
}

fn wrap_spans(
    spans: Vec<Span<'static>>,
    width: usize,
    first_prefix: &str,
    continuation_prefix: &str,
) -> Vec<Line<'static>> {
    if spans.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut current_len = 0usize;
    let mut first_line = true;

    let start_line = |line: &mut Vec<Span<'static>>, len: &mut usize, prefix: &str| {
        if !prefix.is_empty() {
            line.push(Span::raw(prefix.to_owned()));
            *len = prefix.chars().count();
        } else {
            *len = 0;
        }
    };

    start_line(&mut current_spans, &mut current_len, first_prefix);

    let break_line = |out: &mut Vec<Line<'static>>,
                      line: &mut Vec<Span<'static>>,
                      len: &mut usize,
                      first: &mut bool| {
        out.push(Line::from(mem::take(line)));
        *first = false;
        let prefix = if *first {
            first_prefix
        } else {
            continuation_prefix
        };
        if !prefix.is_empty() {
            line.push(Span::raw(prefix.to_owned()));
            *len = prefix.chars().count();
        } else {
            *len = 0;
        }
    };

    for span in spans {
        let style = span.style;
        let text = span.content.into_owned();
        for token in tokenize_spaces(&text) {
            let token_len = token.chars().count();
            let prefix_len = if first_line {
                first_prefix.chars().count()
            } else {
                continuation_prefix.chars().count()
            };

            if token.trim().is_empty() && current_len <= prefix_len {
                continue;
            }

            if token_len > width.saturating_sub(prefix_len) {
                if current_len > prefix_len {
                    break_line(
                        &mut out,
                        &mut current_spans,
                        &mut current_len,
                        &mut first_line,
                    );
                }
                let mut chunk = String::new();
                for ch in token.chars() {
                    if current_len >= width {
                        if !chunk.is_empty() {
                            current_spans.push(Span::styled(mem::take(&mut chunk), style));
                        }
                        break_line(
                            &mut out,
                            &mut current_spans,
                            &mut current_len,
                            &mut first_line,
                        );
                    }
                    chunk.push(ch);
                    current_len += 1;
                }
                if !chunk.is_empty() {
                    current_spans.push(Span::styled(chunk, style));
                }
                continue;
            }

            if current_len > prefix_len && current_len + token_len > width {
                break_line(
                    &mut out,
                    &mut current_spans,
                    &mut current_len,
                    &mut first_line,
                );
            }

            current_len += token_len;
            current_spans.push(Span::styled(token, style));
        }
    }

    if !current_spans.is_empty() {
        out.push(Line::from(current_spans));
    }

    out
}

fn tokenize_spaces(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_is_space = None;
    for ch in text.chars() {
        let is_space = ch == ' ';
        if current_is_space == Some(is_space) || current.is_empty() {
            current.push(ch);
            current_is_space = Some(is_space);
            continue;
        }
        out.push(mem::take(&mut current));
        current.push(ch);
        current_is_space = Some(is_space);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

pub(crate) fn render_markdown(source: &str, width: u16) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::new(width);
    let parser = Parser::new_ext(source, Options::empty());

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    renderer.ensure_block_spacing();
                }
                Tag::Heading { .. } => {
                    renderer.ensure_block_spacing();
                    let next = renderer.current_style.add_modifier(Modifier::BOLD);
                    renderer.style_stack.push(next);
                    renderer.current_style = next;
                }
                Tag::Strong => {
                    let next = renderer.current_style.add_modifier(Modifier::BOLD);
                    renderer.style_stack.push(next);
                    renderer.current_style = next;
                }
                Tag::Emphasis => {
                    let next = renderer.current_style.add_modifier(Modifier::ITALIC);
                    renderer.style_stack.push(next);
                    renderer.current_style = next;
                }
                Tag::CodeBlock(kind) => {
                    renderer.ensure_block_spacing();
                    renderer.flush_current_as_wrapped();
                    renderer.in_code_block = true;
                    renderer.code_lang = match kind {
                        CodeBlockKind::Indented => String::new(),
                        CodeBlockKind::Fenced(lang) => lang.into_string(),
                    };
                    renderer.code_highlighter = renderer.build_code_highlighter();
                    renderer.push_code_block_header();
                }
                Tag::List(None) => {
                    renderer.list_depth = renderer.list_depth.saturating_add(1);
                    renderer.ensure_block_spacing();
                }
                Tag::Item => {
                    renderer.flush_current_as_wrapped();
                    renderer.in_list_item = true;
                    let indent = "  ".repeat(renderer.list_depth.max(1) - 1);
                    renderer.current_item_prefix = format!("{}  • ", indent);
                    renderer.current_item_continuation_prefix = format!("{}    ", indent);
                }
                Tag::Link { dest_url, .. } => {
                    renderer.link_target = Some(dest_url.into_string());
                    renderer.link_text.clear();
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    renderer.flush_current_as_wrapped();
                    renderer.pending_paragraph_blank = true;
                }
                TagEnd::Heading(_) => {
                    renderer.flush_current_as_wrapped();
                    renderer.pending_paragraph_blank = true;
                    renderer.style_stack.pop();
                    renderer.current_style =
                        *renderer.style_stack.last().unwrap_or(&Style::default());
                }
                TagEnd::Strong | TagEnd::Emphasis => {
                    renderer.style_stack.pop();
                    renderer.current_style =
                        *renderer.style_stack.last().unwrap_or(&Style::default());
                }
                TagEnd::CodeBlock => {
                    renderer.in_code_block = false;
                    renderer.code_highlighter = None;
                    renderer.push_code_block_footer();
                    renderer.pending_paragraph_blank = true;
                    renderer.code_lang.clear();
                }
                TagEnd::Item => {
                    renderer.flush_current_as_wrapped();
                    renderer.in_list_item = false;
                    renderer.current_item_prefix.clear();
                    renderer.current_item_continuation_prefix.clear();
                }
                TagEnd::List(_) => {
                    renderer.list_depth = renderer.list_depth.saturating_sub(1);
                    renderer.pending_paragraph_blank = true;
                }
                TagEnd::Link => {
                    if let Some(target) = renderer.link_target.take() {
                        let combined = if renderer.link_text.is_empty() {
                            target.clone()
                        } else {
                            format!("{} ({})", renderer.link_text, target)
                        };
                        renderer.push_span(combined, Style::default().fg(Color::Cyan));
                    }
                    renderer.link_text.clear();
                }
                _ => {}
            },
            Event::Text(text) => renderer.push_text(&text),
            Event::Code(text) => {
                if renderer.link_target.is_some() {
                    renderer.link_text.push_str(&text);
                } else {
                    renderer.push_span(text.into_string(), Style::default().fg(Color::Magenta));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                renderer.flush_current_as_wrapped();
            }
            _ => {}
        }
    }

    renderer.finalize()
}

pub(crate) fn render_markdown_streaming(buffer: &str) -> (Vec<Line<'static>>, usize) {
    let Some(last_newline) = buffer.rfind('\n') else {
        return (Vec::new(), 0);
    };

    let commit_end = last_newline + 1;
    let rendered = render_markdown(&buffer[..commit_end], u16::MAX);
    (rendered, commit_end)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::{render_markdown, render_markdown_streaming};

    fn line_to_text(line: &ratatui::text::Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn plain_text_renders_as_single_line() {
        let lines = render_markdown("hello world", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_to_text(&lines[0]), "hello world");
    }

    #[test]
    fn bold_text_gets_bold_modifier() {
        let lines = render_markdown("**bold**", 80);
        let has_bold = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            span.content.as_ref().contains("bold")
                && span.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(has_bold);
    }

    #[test]
    fn inline_code_gets_magenta_color() {
        let lines = render_markdown("before `code` after", 80);
        let has_code = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            span.content.as_ref().contains("code") && span.style.fg == Some(Color::Magenta)
        });
        assert!(has_code);
    }

    #[test]
    fn code_block_gets_pipe_prefix() {
        let source = "```rust\nlet x = 1;\n```";
        let lines = render_markdown(source, 80);
        let has_pipe = lines
            .iter()
            .map(line_to_text)
            .any(|text| text.starts_with("│ "));
        assert!(has_pipe);
    }

    #[test]
    fn code_block_with_lang_gets_colored_spans() {
        let source = "```rust\nlet x = 1;\n```";
        let lines = render_markdown(source, 80);
        let has_colored_span = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            matches!(span.style.fg, Some(Color::Rgb(_, _, _)))
                && span.content.as_ref().chars().any(|ch| !ch.is_whitespace())
        });
        assert!(has_colored_span);
    }

    #[test]
    fn code_block_without_lang_uses_fallback() {
        let source = "```\nplain text\n```";
        let lines = render_markdown(source, 80);
        let has_dark_gray_plain = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            span.content.as_ref().contains("plain text") && span.style.fg == Some(Color::DarkGray)
        });
        assert!(has_dark_gray_plain);
    }

    #[test]
    fn headings_get_bold_style() {
        let lines = render_markdown("# heading", 80);
        let has_bold_heading = lines.iter().flat_map(|line| line.spans.iter()).any(|span| {
            span.content.as_ref().contains("heading")
                && span.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(has_bold_heading);
    }

    #[test]
    fn list_items_get_bullet_prefix() {
        let lines = render_markdown("- one", 80);
        let texts = lines.iter().map(line_to_text).collect::<Vec<_>>();
        assert!(texts.iter().any(|text| text.starts_with("  • ")));
    }

    #[test]
    fn paragraphs_have_blank_line_between_them() {
        let lines = render_markdown("first\n\nsecond", 80);
        let texts = lines.iter().map(line_to_text).collect::<Vec<_>>();
        assert_eq!(texts, vec!["first", "", "second"]);
    }

    #[test]
    fn streaming_only_returns_lines_before_last_newline() {
        let (lines, consumed) = render_markdown_streaming("first\nsecond\npartial");
        let texts = lines.iter().map(line_to_text).collect::<Vec<_>>();
        assert_eq!(texts, vec!["first", "second"]);
        assert_eq!(consumed, "first\nsecond\n".len());
    }

    #[test]
    fn streaming_with_no_newline_returns_empty() {
        let (lines, consumed) = render_markdown_streaming("partial");
        assert!(lines.is_empty());
        assert_eq!(consumed, 0);
    }
}
