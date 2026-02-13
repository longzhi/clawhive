use anyhow::Result;
use chrono::NaiveDate;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

/// Manages MEMORY.md and memory/YYYY-MM-DD.md files
pub struct MemoryFileStore {
    workspace: PathBuf,
}

impl MemoryFileStore {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            workspace: workspace.as_ref().to_path_buf(),
        }
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
        fs::write(self.long_term_path(), content).await?;
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

        if fs::metadata(&path).await.is_err() {
            fs::write(&path, format!("# {}\n\n", date.format("%Y-%m-%d"))).await?;
        }

        let mut file = fs::OpenOptions::new().append(true).open(path).await?;
        file.write_all(format!("\n{content}\n").as_bytes()).await?;
        Ok(())
    }

    /// Overwrite a daily file (used by hippocampus)
    pub async fn write_daily(&self, date: NaiveDate, content: &str) -> Result<()> {
        self.ensure_daily_dir().await?;
        fs::write(self.daily_path(date), content).await?;
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
        let long_term_truncated: String = long_term.chars().take(2000).collect();

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
    use super::MemoryFileStore;
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

        let long_term = "A".repeat(2500);
        store.write_long_term(&long_term).await?;
        store.write_daily(date(2026, 2, 11), "daily-1").await?;
        store.write_daily(date(2026, 2, 12), "daily-2").await?;
        store.write_daily(date(2026, 2, 13), "daily-3").await?;
        store.write_daily(date(2026, 2, 14), "daily-4").await?;

        let ctx = store.build_memory_context().await?;
        assert!(ctx.starts_with("[Memory Context]\n\nFrom MEMORY.md:\n"));
        assert!(ctx.contains(&"A".repeat(2000)));
        assert!(!ctx.contains(&"A".repeat(2001)));
        assert!(ctx.contains("From memory/2026-02-14.md:\n"));
        assert!(ctx.contains("From memory/2026-02-13.md:\n"));
        assert!(ctx.contains("From memory/2026-02-12.md:\n"));
        assert!(!ctx.contains("From memory/2026-02-11.md:\n"));
        Ok(())
    }
}
