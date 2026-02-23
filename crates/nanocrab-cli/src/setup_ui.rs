use console::{style, Emoji, Term};

pub static CHECKMARK: Emoji<'_, '_> = Emoji("✅ ", "√ ");
pub static ARROW: Emoji<'_, '_> = Emoji("➜  ", "-> ");
pub static CRAB: Emoji<'_, '_> = Emoji("🦀 ", "");

pub fn print_logo(term: &Term) {
    let logo = r#"
  ┌─────────────────────────┐
  │    nanocrab  setup      │
  └─────────────────────────┘
"#;
    let _ = term.write_line(&format!("{}", style(logo).cyan()));
}

pub fn print_step(term: &Term, current: usize, total: usize, title: &str) {
    let _ = term.write_line(&format!(
        "\n{} {}",
        style(format!("[{}/{}]", current, total)).bold().cyan(),
        style(title).bold()
    ));
}

pub fn print_done(term: &Term, msg: &str) {
    let _ = term.write_line(&format!("{} {}", CHECKMARK, style(msg).green()));
}
