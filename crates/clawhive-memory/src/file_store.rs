use anyhow::Result;
use chrono::NaiveDate;
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::safe_io;

fn smart_truncate(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }

    let mut sections = Vec::new();
    let mut current = String::new();

    for line in content.split_inclusive('\n') {
        let is_heading = line.starts_with("# ") || line.starts_with("## ");
        if is_heading && !current.is_empty() {
            sections.push(current);
            current = String::new();
        }
        current.push_str(line);
    }

    if !current.is_empty() {
        sections.push(current);
    }

    let mut kept = String::new();
    for section in &sections {
        if kept.chars().count() + section.chars().count() > max_chars {
            if kept.is_empty() {
                return truncate_at_last_newline(content, max_chars);
            }

            return append_truncation_marker(&kept);
        }

        kept.push_str(section);
    }

    append_truncation_marker(&kept)
}

fn truncate_at_last_newline(content: &str, max_chars: usize) -> String {
    let cutoff = byte_index_at_char_limit(content, max_chars);
    let truncated = content[..cutoff]
        .rfind('\n')
        .map(|idx| &content[..idx])
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or(&content[..cutoff]);

    append_truncation_marker(truncated)
}

fn byte_index_at_char_limit(content: &str, max_chars: usize) -> usize {
    content
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(content.len())
}

fn append_truncation_marker(content: &str) -> String {
    let trimmed = content.trim_end_matches('\n');
    format!("{trimmed}\n...[truncated]")
}

/// Manages MEMORY.md and memory/YYYY-MM-DD.md files
#[derive(Clone)]
pub struct MemoryFileStore {
    workspace: PathBuf,
}

impl MemoryFileStore {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            workspace: workspace.as_ref().to_path_buf(),
        }
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace
    }

    /// Read the entire MEMORY.md content. Returns empty string if file doesn't exist.
    pub async fn read_long_term(&self) -> Result<String> {
        let path = self.long_term_path();
        match fs::read_to_string(path).await {
            Ok(content) => Ok(content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(err) => Err(err.into()),
        }
    }

    /// Overwrite MEMORY.md with new content (used by hippocampus consolidation)
    pub async fn write_long_term(&self, content: &str) -> Result<()> {
        safe_io::safe_overwrite(&self.long_term_path(), content.as_bytes()).await?;
        Ok(())
    }

    /// Read a specific daily file. Returns None if file doesn't exist.
    pub async fn read_daily(&self, date: NaiveDate) -> Result<Option<String>> {
        let path = self.daily_path(date);
        match fs::read_to_string(path).await {
            Ok(content) => Ok(Some(content)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Append content to today's daily file. Creates file with "# YYYY-MM-DD" header if new.
    pub async fn append_daily(&self, date: NaiveDate, content: &str) -> Result<()> {
        self.ensure_daily_dir().await?;
        let path = self.daily_path(date);

        let header = format!("# {}\n\n", date.format("%Y-%m-%d"));
        let body = format!("\n{content}\n");

        safe_io::locked_append_with_header(&path, header.as_bytes(), body.as_bytes()).await?;
        Ok(())
    }

    /// Overwrite a daily file (used by hippocampus)
    pub async fn write_daily(&self, date: NaiveDate, content: &str) -> Result<()> {
        self.ensure_daily_dir().await?;
        safe_io::atomic_write(&self.daily_path(date), content.as_bytes()).await?;
        Ok(())
    }

    /// List all daily files sorted by date (newest first), returns (date, path) tuples
    pub async fn list_daily_files(&self) -> Result<Vec<(NaiveDate, PathBuf)>> {
        let mut out = Vec::new();
        let daily_dir = self.daily_dir();

        if fs::metadata(&daily_dir).await.is_err() {
            return Ok(out);
        }

        let mut entries = fs::read_dir(&daily_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }

            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if !name.ends_with(".md") || name.len() != 13 {
                continue;
            }

            let date_part = &name[..10];
            if let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
                out.push((date, entry.path()));
            }
        }

        out.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(out)
    }

    /// Read recent N days of daily files, returns Vec<(date, content)>
    pub async fn read_recent_daily(&self, days: usize) -> Result<Vec<(NaiveDate, String)>> {
        let files = self.list_daily_files().await?;
        let mut out = Vec::new();

        for (date, path) in files.into_iter().take(days) {
            let content = fs::read_to_string(path).await?;
            out.push((date, content));
        }

        Ok(out)
    }

    pub async fn build_memory_context(&self) -> Result<String> {
        let long_term = self.read_long_term().await?;
        let long_term_truncated = smart_truncate(&long_term, 4000);

        let mut sections = vec![
            "[Memory Context]".to_string(),
            String::new(),
            "From MEMORY.md:".to_string(),
            long_term_truncated,
        ];

        for (date, content) in self.read_recent_daily(3).await? {
            sections.push(String::new());
            sections.push(format!("From memory/{}.md:", date.format("%Y-%m-%d")));
            sections.push(content);
        }

        Ok(sections.join("\n"))
    }

    fn long_term_path(&self) -> PathBuf {
        self.workspace.join("MEMORY.md")
    }

    fn daily_dir(&self) -> PathBuf {
        self.workspace.join("memory")
    }

    fn daily_path(&self, date: NaiveDate) -> PathBuf {
        self.daily_dir()
            .join(format!("{}.md", date.format("%Y-%m-%d")))
    }

    async fn ensure_daily_dir(&self) -> Result<()> {
        fs::create_dir_all(self.daily_dir()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{smart_truncate, MemoryFileStore};
    use anyhow::Result;
    use chrono::NaiveDate;
    use tempfile::TempDir;
    use tokio::fs;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[tokio::test]
    async fn test_read_long_term_empty() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        let content = store.read_long_term().await?;
        assert!(content.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_write_and_read_long_term() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        store.write_long_term("long term memory").await?;
        let content = store.read_long_term().await?;
        assert_eq!(content, "long term memory");
        Ok(())
    }

    #[tokio::test]
    async fn test_write_long_term_creates_backup() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        store.write_long_term("old memory").await?;
        store.write_long_term("new memory").await?;

        let backup = fs::read_to_string(dir.path().join("MEMORY.md.bak")).await?;
        assert_eq!(backup, "old memory");
        Ok(())
    }

    #[tokio::test]
    async fn test_read_daily_none() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        let content = store.read_daily(date(2026, 2, 13)).await?;
        assert_eq!(content, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_append_daily_creates_file() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());
        let d = date(2026, 2, 13);

        store.append_daily(d, "first entry").await?;

        let file = dir.path().join("memory").join("2026-02-13.md");
        let content = fs::read_to_string(file).await?;
        assert!(content.starts_with("# 2026-02-13\n\n"));
        assert!(content.contains("\nfirst entry\n"));
        Ok(())
    }

    #[tokio::test]
    async fn test_append_daily_appends() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());
        let d = date(2026, 2, 13);

        store.append_daily(d, "entry one").await?;
        store.append_daily(d, "entry two").await?;

        let content = store
            .read_daily(d)
            .await?
            .expect("daily file should exist after append");
        assert!(content.contains("entry one"));
        assert!(content.contains("entry two"));
        Ok(())
    }

    #[tokio::test]
    async fn test_append_daily_prepends_header_for_empty_existing_file() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());
        let d = date(2026, 2, 13);
        let file = dir.path().join("memory").join("2026-02-13.md");

        fs::create_dir_all(file.parent().expect("daily file parent")).await?;
        fs::write(&file, "").await?;

        store.append_daily(d, "first entry").await?;

        let content = fs::read_to_string(file).await?;
        assert!(content.starts_with("# 2026-02-13\n\n"));
        assert!(content.contains("\nfirst entry\n"));
        Ok(())
    }

    #[tokio::test]
    async fn test_write_daily_overwrites() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());
        let d = date(2026, 2, 13);

        store.append_daily(d, "old").await?;
        store.write_daily(d, "new content").await?;

        let content = store
            .read_daily(d)
            .await?
            .expect("daily file should exist after write");
        assert_eq!(content, "new content");
        Ok(())
    }

    #[tokio::test]
    async fn test_list_daily_files_sorted() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        store.write_daily(date(2026, 2, 10), "a").await?;
        store.write_daily(date(2026, 2, 12), "b").await?;
        store.write_daily(date(2026, 2, 11), "c").await?;

        let files = store.list_daily_files().await?;
        let dates: Vec<_> = files.into_iter().map(|(d, _)| d).collect();

        assert_eq!(
            dates,
            vec![date(2026, 2, 12), date(2026, 2, 11), date(2026, 2, 10)]
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_read_recent_daily() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        store.write_daily(date(2026, 2, 10), "d1").await?;
        store.write_daily(date(2026, 2, 11), "d2").await?;
        store.write_daily(date(2026, 2, 12), "d3").await?;
        store.write_daily(date(2026, 2, 13), "d4").await?;

        let recent = store.read_recent_daily(2).await?;
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0], (date(2026, 2, 13), "d4".to_string()));
        assert_eq!(recent[1], (date(2026, 2, 12), "d3".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn test_build_memory_context() -> Result<()> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());

        let long_term = [
            "# Profile\nAlpha\n",
            "## Goals\n",
            &"A".repeat(3970),
            "\n",
            "## Later\nBeta\n",
        ]
        .concat();
        store.write_long_term(&long_term).await?;
        store.write_daily(date(2026, 2, 11), "daily-1").await?;
        store.write_daily(date(2026, 2, 12), "daily-2").await?;
        store.write_daily(date(2026, 2, 13), "daily-3").await?;
        store.write_daily(date(2026, 2, 14), "daily-4").await?;

        let ctx = store.build_memory_context().await?;
        assert!(ctx.starts_with("[Memory Context]\n\nFrom MEMORY.md:\n"));
        assert!(ctx.contains("# Profile\nAlpha\n"));
        assert!(ctx.contains("## Goals\n"));
        assert!(ctx.contains("...[truncated]"));
        assert!(!ctx.contains("## Later\nBeta\n"));
        assert!(ctx.contains("From memory/2026-02-14.md:\n"));
        assert!(ctx.contains("From memory/2026-02-13.md:\n"));
        assert!(ctx.contains("From memory/2026-02-12.md:\n"));
        assert!(!ctx.contains("From memory/2026-02-11.md:\n"));
        Ok(())
    }

    #[test]
    fn test_smart_truncate_short_content() {
        let content = "# Title\nshort\n";

        assert_eq!(smart_truncate(content, 4000), content);
    }

    #[test]
    fn test_smart_truncate_splits_at_headings() {
        let content = "# First\nKeep\n\n## Second\nAlso keep\n\n## Third\nDrop\n";

        assert_eq!(
            smart_truncate(content, 35),
            "# First\nKeep\n\n## Second\nAlso keep\n...[truncated]"
        );
    }

    #[test]
    fn test_smart_truncate_single_large_section() {
        let content = ["# Large\n", &"A".repeat(5000), "\nnext line\n"].concat();
        let truncated = smart_truncate(&content, 4000);

        assert!(truncated.ends_with("\n...[truncated]"));
        assert!(truncated.len() <= 4000 + "\n...[truncated]".len());
        assert!(!truncated.contains("next line"));
    }

    #[test]
    fn test_smart_truncate_adds_marker() {
        let content = "# One\nAlpha\n\n## Two\nBeta\n";

        assert!(smart_truncate(content, 12).ends_with("\n...[truncated]"));
    }
}
