use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ScheduleState {
    pub schedule_id: String,
    pub next_run_at_ms: Option<i64>,
    pub running_at_ms: Option<i64>,
    pub last_run_at_ms: Option<i64>,
    pub last_run_status: Option<RunStatus>,
    pub last_error: Option<String>,
    pub last_duration_ms: Option<u64>,
    pub consecutive_errors: u32,
    #[serde(default)]
    pub last_delivery_status: Option<DeliveryStatus>,
    #[serde(default)]
    pub last_delivery_error: Option<String>,
}

impl ScheduleState {
    pub fn new(schedule_id: impl Into<String>) -> Self {
        Self {
            schedule_id: schedule_id.into(),
            next_run_at_ms: None,
            running_at_ms: None,
            last_run_at_ms: None,
            last_run_status: None,
            last_error: None,
            last_duration_ms: None,
            consecutive_errors: 0,
            last_delivery_status: None,
            last_delivery_error: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum RunStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "skipped")]
    Skipped,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    #[serde(rename = "delivered")]
    Delivered,
    #[serde(rename = "not_delivered")]
    NotDelivered,
    #[serde(rename = "not_requested")]
    NotRequested,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub schedule_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub status: RunStatus,
    pub error: Option<String>,
    pub duration_ms: u64,
    #[serde(default)]
    pub response: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_status_serde_roundtrip() {
        let state = ScheduleState {
            schedule_id: "test".into(),
            next_run_at_ms: None,
            running_at_ms: None,
            last_run_at_ms: None,
            last_run_status: None,
            last_error: None,
            last_duration_ms: None,
            consecutive_errors: 0,
            last_delivery_status: Some(DeliveryStatus::Delivered),
            last_delivery_error: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ScheduleState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_delivery_status, Some(DeliveryStatus::Delivered));
    }
}
