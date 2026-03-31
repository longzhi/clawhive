const SECTION_PROJECTS: &str = "长期项目主线";
const SECTION_CONTEXT: &str = "持续性背景脉络";
const SECTION_DECISIONS: &str = "关键历史决策";

pub const MEMORY_SECTION_ORDER: [&str; 3] = [SECTION_PROJECTS, SECTION_CONTEXT, SECTION_DECISIONS];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySection {
    pub heading: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDocument {
    preface: String,
    sections: Vec<MemorySection>,
}

impl MemoryDocument {
    pub fn parse(raw: &str) -> Self {
        let mut preface = String::new();
        let mut sections = Vec::new();
        let mut current_heading: Option<String> = None;
        let mut current_body = String::new();

        for line in raw.lines() {
            if let Some(heading) = line.strip_prefix("## ") {
                flush_section(&mut sections, current_heading.take(), &mut current_body);
                current_heading = Some(heading.trim().to_string());
                continue;
            }

            if current_heading.is_some() {
                current_body.push_str(line);
                current_body.push('\n');
            } else {
                preface.push_str(line);
                preface.push('\n');
            }
        }

        flush_section(&mut sections, current_heading.take(), &mut current_body);

        let mut doc = Self {
            preface: preface.trim_end().to_string(),
            sections,
        };
        for heading in MEMORY_SECTION_ORDER {
            doc.ensure_section(heading);
        }
        doc
    }

    pub fn section_content(&self, heading: &str) -> String {
        self.sections
            .iter()
            .find(|section| section.heading == heading)
            .map(|section| section.body.clone())
            .unwrap_or_default()
    }

    pub fn section_items(&self, heading: &str) -> Vec<String> {
        self.sections
            .iter()
            .find(|section| section.heading == heading)
            .map(|section| parse_section_items(&section.body))
            .unwrap_or_default()
    }

    pub fn replace_section(&mut self, heading: &str, content: &str) {
        self.ensure_section(heading);
        if let Some(section) = self
            .sections
            .iter_mut()
            .find(|section| section.heading == heading)
        {
            section.body = content.trim().to_string();
        }
    }

    pub fn render(&self) -> String {
        let mut out = Vec::new();

        if !self.preface.trim().is_empty() {
            out.push(self.preface.trim().to_string());
            out.push(String::new());
        } else {
            out.push("# MEMORY.md".to_string());
            out.push(String::new());
        }

        let mut emitted = std::collections::BTreeSet::new();
        for heading in MEMORY_SECTION_ORDER {
            emitted.insert(heading.to_string());
            out.push(format!("## {heading}"));
            out.push(String::new());
            let body = self
                .sections
                .iter()
                .find(|section| section.heading == heading)
                .map(|section| section.body.trim().to_string())
                .unwrap_or_default();
            if !body.is_empty() {
                out.push(body);
                out.push(String::new());
            } else {
                out.push(String::new());
            }
        }

        for section in &self.sections {
            if emitted.contains(&section.heading) {
                continue;
            }
            out.push(format!("## {}", section.heading));
            out.push(String::new());
            if !section.body.trim().is_empty() {
                out.push(section.body.trim().to_string());
                out.push(String::new());
            }
        }

        while matches!(out.last(), Some(last) if last.is_empty()) {
            out.pop();
        }
        out.push(String::new());
        out.join("\n")
    }

    fn ensure_section(&mut self, heading: &str) {
        if self
            .sections
            .iter()
            .any(|section| section.heading == heading)
        {
            return;
        }
        self.sections.push(MemorySection {
            heading: heading.to_string(),
            body: String::new(),
        });
    }
}

fn flush_section(sections: &mut Vec<MemorySection>, heading: Option<String>, body: &mut String) {
    let Some(heading) = heading else {
        body.clear();
        return;
    };
    sections.push(MemorySection {
        heading,
        body: body.trim().to_string(),
    });
    body.clear();
}

fn parse_section_items(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current: Option<String> = None;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_item(&mut items, &mut current);
            continue;
        }

        if let Some(content) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush_item(&mut items, &mut current);
            current = Some(content.trim().to_string());
            continue;
        }

        if let Some(existing) = current.as_mut() {
            if !existing.is_empty() {
                existing.push(' ');
            }
            existing.push_str(trimmed);
        } else {
            current = Some(trimmed.to_string());
        }
    }

    flush_item(&mut items, &mut current);
    items
}

fn flush_item(items: &mut Vec<String>, current: &mut Option<String>) {
    let Some(item) = current.take() else {
        return;
    };
    let normalized = item.trim();
    if normalized.is_empty() {
        return;
    }
    items.push(normalized.to_string());
}

#[cfg(test)]
mod tests {
    use super::{MemoryDocument, MEMORY_SECTION_ORDER};

    #[test]
    fn parse_preserves_preface_and_adds_missing_sections() {
        let doc = MemoryDocument::parse("# Title\n\nIntro");
        let rendered = doc.render();
        assert!(rendered.contains("# Title"));
        for section in MEMORY_SECTION_ORDER {
            assert!(rendered.contains(&format!("## {section}")));
        }
    }

    #[test]
    fn replace_section_updates_target_section_only() {
        let mut doc = MemoryDocument::parse("# Title\n\n## 长期项目主线\n\nold\n");
        doc.replace_section("长期项目主线", "- new");
        assert_eq!(doc.section_content("长期项目主线"), "- new");
        assert!(doc.render().contains("## 持续性背景脉络"));
    }

    #[test]
    fn section_items_extracts_bullets_and_continuations() {
        let doc = MemoryDocument::parse(
            "# MEMORY.md\n\n## 长期项目主线\n\n- First item\n  continued line\n- Second item\n\nParagraph item\n",
        );
        assert_eq!(
            doc.section_items("长期项目主线"),
            vec![
                "First item continued line".to_string(),
                "Second item".to_string(),
                "Paragraph item".to_string()
            ]
        );
    }
}
