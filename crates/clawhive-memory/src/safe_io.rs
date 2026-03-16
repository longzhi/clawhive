use anyhow::Result;
use fs2::FileExt;
use std::fs;
use std::io::Write;
use std::path::Path;

pub async fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let tmp_path = path.with_extension("tmp");

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&tmp_path, content).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

pub async fn locked_append(path: &Path, content: &[u8]) -> Result<()> {
    let path = path.to_path_buf();
    let content = content.to_vec();

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        file.lock_exclusive()?;

        let write_result = (|| -> Result<()> {
            file.write_all(&content)?;
            file.flush()?;
            Ok(())
        })();

        let unlock_result = file.unlock();
        write_result?;
        unlock_result?;

        Ok(())
    })
    .await??;

    Ok(())
}

pub async fn safe_overwrite(path: &Path, content: &[u8]) -> Result<()> {
    if tokio::fs::try_exists(path).await? {
        let backup_path = path.with_extension("md.bak");
        tokio::fs::copy(path, backup_path).await?;
    }

    atomic_write(path, content).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::Result;
    use tempfile::TempDir;
    use tokio::fs;
    use tokio::task::JoinSet;

    use super::{atomic_write, locked_append, safe_overwrite};

    async fn read(path: &Path) -> Result<Vec<u8>> {
        Ok(fs::read(path).await?)
    }

    #[tokio::test]
    async fn atomic_write_creates_file() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("memory.md");

        atomic_write(&path, b"hello world").await?;

        assert_eq!(read(&path).await?, b"hello world");
        Ok(())
    }

    #[tokio::test]
    async fn atomic_write_no_leftover_tmp() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("memory.md");
        let tmp_path = path.with_extension("tmp");

        atomic_write(&path, b"hello world").await?;

        assert!(!tmp_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn atomic_write_overwrites_existing() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("memory.md");
        fs::write(&path, b"old content").await?;

        atomic_write(&path, b"new content").await?;

        assert_eq!(read(&path).await?, b"new content");
        Ok(())
    }

    #[tokio::test]
    async fn locked_append_creates_file_if_missing() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("session.jsonl");

        locked_append(&path, b"first line\n").await?;

        assert_eq!(read(&path).await?, b"first line\n");
        Ok(())
    }

    #[tokio::test]
    async fn locked_append_appends_to_existing() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("session.jsonl");
        fs::write(&path, b"first line\n").await?;

        locked_append(&path, b"second line\n").await?;

        assert_eq!(read(&path).await?, b"first line\nsecond line\n");
        Ok(())
    }

    #[tokio::test]
    async fn locked_append_concurrent_writes_no_corruption() -> Result<()> {
        let dir = TempDir::new()?;
        let path = Arc::new(dir.path().join("session.jsonl"));
        let mut tasks = JoinSet::new();

        for index in 0..20 {
            let path = Arc::clone(&path);
            tasks.spawn(async move {
                let line = format!("line-{index:02}\n");
                locked_append(path.as_ref(), line.as_bytes()).await
            });
        }

        while let Some(result) = tasks.join_next().await {
            result??;
        }

        let content = String::from_utf8(read(path.as_ref()).await?)?;
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 20);

        let expected: HashSet<String> = (0..20).map(|index| format!("line-{index:02}")).collect();
        let actual: HashSet<String> = lines.iter().map(|line| (*line).to_owned()).collect();
        assert_eq!(actual, expected);

        Ok(())
    }

    #[tokio::test]
    async fn safe_overwrite_creates_backup() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("MEMORY.md");
        let backup_path = path.with_extension("md.bak");
        fs::write(&path, b"old memory").await?;

        safe_overwrite(&path, b"new memory").await?;

        assert_eq!(read(&path).await?, b"new memory");
        assert_eq!(read(&backup_path).await?, b"old memory");
        Ok(())
    }

    #[tokio::test]
    async fn safe_overwrite_works_when_no_existing_file() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("MEMORY.md");
        let backup_path = path.with_extension("md.bak");

        safe_overwrite(&path, b"new memory").await?;

        assert_eq!(read(&path).await?, b"new memory");
        assert!(!backup_path.exists());
        Ok(())
    }
}
