use std::sync::Arc;

use chrono::Utc;
use nanocrab_bus::{EventBus, Topic};
use nanocrab_scheduler::{
    apply_job_result, error_backoff_ms, CompletedResult, DeliveryConfig, RunStatus, ScheduleConfig,
    ScheduleEntry, ScheduleManager, ScheduleState, ScheduleType, SessionMode,
};
use nanocrab_schema::BusMessage;
use tokio::time::{timeout, Duration};

#[test]
fn cron_schedule_config_loads_with_defaults() {
    let yaml = r#"
schedule_id: test-daily
enabled: true
name: "Test Daily"
schedule:
  kind: cron
  expr: "0 9 * * *"
  tz: "Asia/Shanghai"
agent_id: nanocrab-main
session_mode: isolated
task: "Test task"
"#;

    let config: ScheduleConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.schedule_id, "test-daily");
    assert!(config.enabled);
    assert!(matches!(config.delivery.mode, nanocrab_scheduler::DeliveryMode::None));
}

#[tokio::test]
async fn schedule_manager_triggers_bus_event() {
    let temp = tempfile::TempDir::new().unwrap();
    let config_dir = temp.path().join("config/schedules.d");
    let data_dir = temp.path().join("data/schedules");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let yaml = r#"
schedule_id: test-immediate
enabled: true
name: "Immediate"
schedule:
  kind: every
  interval_ms: 100
agent_id: test-agent
session_mode: isolated
task: "Hello from test"
"#;
    std::fs::write(config_dir.join("test-immediate.yaml"), yaml).unwrap();

    let bus = Arc::new(EventBus::new(32));
    let mut rx = bus.subscribe(Topic::ScheduledTaskTriggered).await;
    let manager = ScheduleManager::new(&config_dir, &data_dir, Arc::clone(&bus)).unwrap();
    let handle = tokio::spawn(async move {
        manager.run().await;
    });

    let msg = timeout(Duration::from_secs(2), rx.recv()).await;
    handle.abort();

    assert!(msg.is_ok());
    assert!(matches!(
        msg.unwrap().unwrap(),
        BusMessage::ScheduledTaskTriggered { schedule_id, .. } if schedule_id == "test-immediate"
    ));
}

#[test]
fn error_backoff_and_state_transition_work() {
    let mut entry = ScheduleEntry {
        config: ScheduleConfig {
            schedule_id: "retry-test".to_string(),
            enabled: true,
            name: "Retry test".to_string(),
            description: None,
            schedule: ScheduleType::Every {
                interval_ms: 1_000,
                anchor_ms: Some(0),
            },
            agent_id: "nanocrab-main".to_string(),
            session_mode: SessionMode::Isolated,
            task: "run".to_string(),
            timeout_seconds: 300,
            delete_after_run: false,
            delivery: DeliveryConfig::default(),
        },
        state: ScheduleState {
            schedule_id: "retry-test".to_string(),
            next_run_at_ms: Some(Utc::now().timestamp_millis() + 1_000),
            running_at_ms: Some(Utc::now().timestamp_millis() - 500),
            last_run_at_ms: None,
            last_run_status: None,
            last_error: None,
            last_duration_ms: None,
            consecutive_errors: 0,
        },
    };

    let result = CompletedResult {
        status: RunStatus::Error,
        error: Some("timeout".to_string()),
        started_at_ms: 1_000,
        ended_at_ms: 2_000,
        duration_ms: 1_000,
    };

    assert_eq!(error_backoff_ms(1), 30_000);
    assert_eq!(error_backoff_ms(5), 3_600_000);

    let should_delete = apply_job_result(&mut entry, &result);
    assert!(!should_delete);
    assert_eq!(entry.state.consecutive_errors, 1);
    assert!(entry.state.next_run_at_ms.unwrap() >= result.ended_at_ms + 30_000);
}
