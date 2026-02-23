use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::io::AsyncWriteExt;

use crate::{RunRecord, ScheduleEntry, ScheduleState};

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("state.json"),
        }
    }

    pub fn load(&self) -> Result<HashMap<String, ScheduleState>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }

        let content = std::fs::read_to_string(&self.path)?;
        Ok(serde_json::from_str(&content)?)
    }

    pub async fn persist(&self, entries: &HashMap<String, ScheduleEntry>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let states: HashMap<&str, &ScheduleState> = entries
            .iter()
            .map(|(k, v)| (k.as_str(), &v.state))
            .collect();
        let json = serde_json::to_string_pretty(&states)?;
        tokio::fs::write(&self.path, json).await?;
        Ok(())
    }
}

pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("runs"),
        }
    }

    pub async fn append(&self, record: &RunRecord) -> Result<()> {
        tokio::fs::create_dir_all(&self.dir).await?;
        let path = self.dir.join(format!("{}.jsonl", record.schedule_id));
        let mut line = serde_json::to_string(record)?;
        line.push('\n');

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub async fn recent(&self, schedule_id: &str, limit: usize) -> Result<Vec<RunRecord>> {
        let path = self.dir.join(format!("{}.jsonl", schedule_id));
        if !path.exists() {
            return Ok(vec![]);
        }

        let content = tokio::fs::read_to_string(&path).await?;
        let records: Vec<RunRecord> = content
            .lines()
            .rev()
            .take(limit)
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(records)
    }
}
