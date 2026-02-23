use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ScheduleConfig {
    pub schedule_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub schedule: ScheduleType,
    pub agent_id: String,
    #[serde(default)]
    pub session_mode: SessionMode,
    pub task: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub delete_after_run: bool,
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            schedule_id: String::new(),
            enabled: default_true(),
            name: String::new(),
            description: None,
            schedule: ScheduleType::At {
                at: "10m".to_string(),
            },
            agent_id: "nanocrab-main".to_string(),
            session_mode: SessionMode::default(),
            task: String::new(),
            timeout_seconds: default_timeout(),
            delete_after_run: false,
            delivery: DeliveryConfig::default(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum ScheduleType {
    #[serde(rename = "cron")]
    Cron {
        expr: String,
        #[serde(default = "default_tz")]
        tz: String,
    },
    #[serde(rename = "at")]
    At { at: String },
    #[serde(rename = "every")]
    Every {
        interval_ms: u64,
        #[serde(default)]
        anchor_ms: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub enum SessionMode {
    #[default]
    #[serde(rename = "isolated")]
    Isolated,
    #[serde(rename = "main")]
    Main,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub mode: DeliveryMode,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub connector_id: Option<String>,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            mode: DeliveryMode::None,
            channel: None,
            connector_id: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    300
}

fn default_tz() -> String {
    "UTC".to_string()
}
