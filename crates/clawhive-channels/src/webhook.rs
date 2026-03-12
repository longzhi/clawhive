use std::collections::HashMap;

use serde_json::Value;
use uuid::Uuid;

const RAW_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub struct NormalizedEvent {
    pub text: String,
    pub title: Option<String>,
    pub severity: Option<String>,
    pub labels: HashMap<String, String>,
}

pub trait PayloadNormalizer: Send + Sync {
    fn normalize(&self, payload: &Value) -> NormalizedEvent;
    fn derive_scope(&self, payload: &Value, source_id: &str) -> String;
}

pub struct RawNormalizer;

impl PayloadNormalizer for RawNormalizer {
    fn normalize(&self, payload: &Value) -> NormalizedEvent {
        let text = truncate_text(pretty_json(payload), RAW_MAX_BYTES);

        NormalizedEvent {
            text,
            title: None,
            severity: None,
            labels: HashMap::new(),
        }
    }

    fn derive_scope(&self, _payload: &Value, source_id: &str) -> String {
        format!("source:{source_id}:event:{}", Uuid::new_v4())
    }
}

pub struct GenericNormalizer;

impl PayloadNormalizer for GenericNormalizer {
    fn normalize(&self, payload: &Value) -> NormalizedEvent {
        let title = payload
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let body = payload
            .get("body")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let labels = object_to_string_map(payload.get("labels").and_then(Value::as_object));
        let text = match (&title, &body) {
            (Some(t), Some(b)) => format!("{t}\n\n{b}"),
            (Some(t), None) => t.clone(),
            (None, Some(b)) => b.clone(),
            (None, None) => truncate_text(pretty_json(payload), RAW_MAX_BYTES),
        };

        NormalizedEvent {
            text,
            title,
            severity: payload
                .get("severity")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| labels.get("severity").cloned()),
            labels,
        }
    }

    fn derive_scope(&self, payload: &Value, source_id: &str) -> String {
        let labels = payload.get("labels").and_then(Value::as_object);
        if let Some(labels) = labels {
            if !labels.is_empty() {
                let mut pairs: Vec<String> = labels
                    .iter()
                    .map(|(k, v)| {
                        let v_str = v
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string());
                        format!("{k}={v_str}")
                    })
                    .collect();
                pairs.sort();
                let scope_key = pairs.join(":");
                return format!("source:{source_id}:event:{scope_key}");
            }
        }

        format!("source:{source_id}:event:{}", Uuid::new_v4())
    }
}

pub struct AlertmanagerNormalizer;

impl PayloadNormalizer for AlertmanagerNormalizer {
    fn normalize(&self, payload: &Value) -> NormalizedEvent {
        let status_default = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let common_labels =
            object_to_string_map(payload.get("commonLabels").and_then(Value::as_object));
        let group_labels =
            object_to_string_map(payload.get("groupLabels").and_then(Value::as_object));
        let common_annotations =
            object_to_string_map(payload.get("commonAnnotations").and_then(Value::as_object));

        let mut lines = Vec::new();
        if let Some(alerts) = payload.get("alerts").and_then(Value::as_array) {
            for alert in alerts {
                let labels = alert.get("labels").and_then(Value::as_object);
                let annotations = alert.get("annotations").and_then(Value::as_object);

                let status = alert
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or(status_default);
                let status_upper = status.to_ascii_uppercase();
                let emoji = match status {
                    "firing" => "🔴",
                    "resolved" => "🟢",
                    _ => "🟡",
                };

                let alertname = labels
                    .and_then(|l| l.get("alertname"))
                    .and_then(Value::as_str)
                    .or_else(|| group_labels.get("alertname").map(String::as_str))
                    .unwrap_or("unknown");
                let severity = labels
                    .and_then(|l| l.get("severity"))
                    .and_then(Value::as_str)
                    .or_else(|| common_labels.get("severity").map(String::as_str))
                    .unwrap_or("unknown");
                let instance = labels
                    .and_then(|l| l.get("instance"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let summary = annotations
                    .and_then(|a| a.get("summary"))
                    .and_then(Value::as_str)
                    .or_else(|| common_annotations.get("summary").map(String::as_str))
                    .unwrap_or("-");
                let started = alert.get("startsAt").and_then(Value::as_str).unwrap_or("-");

                lines.push(format!(
                    "[Alertmanager] {emoji} {status_upper}: {alertname}\nSeverity: {severity}\nInstance: {instance}\nSummary: {summary}\nStarted: {started}"
                ));
            }
        }

        let text = if lines.is_empty() {
            truncate_text(pretty_json(payload), RAW_MAX_BYTES)
        } else {
            lines.join("\n\n")
        };

        let mut labels = common_labels;
        labels.extend(group_labels);
        if let Some(first_alert_labels) = payload
            .get("alerts")
            .and_then(Value::as_array)
            .and_then(|alerts| alerts.first())
            .and_then(|alert| alert.get("labels"))
            .and_then(Value::as_object)
        {
            labels.extend(object_to_string_map(Some(first_alert_labels)));
        }

        let severity = payload
            .get("alerts")
            .and_then(Value::as_array)
            .and_then(|alerts| alerts.first())
            .and_then(|alert| alert.get("labels"))
            .and_then(Value::as_object)
            .and_then(|labels| labels.get("severity"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| labels.get("severity").cloned());

        let title = payload
            .get("alerts")
            .and_then(Value::as_array)
            .and_then(|alerts| alerts.first())
            .and_then(|alert| alert.get("labels"))
            .and_then(Value::as_object)
            .and_then(|labels| labels.get("alertname"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| labels.get("alertname").cloned());

        NormalizedEvent {
            text,
            title,
            severity,
            labels,
        }
    }

    fn derive_scope(&self, payload: &Value, source_id: &str) -> String {
        if let Some(first_alert) = payload
            .get("alerts")
            .and_then(Value::as_array)
            .and_then(|alerts| alerts.first())
            .and_then(Value::as_object)
        {
            let alertname = first_alert
                .get("labels")
                .and_then(Value::as_object)
                .and_then(|labels| labels.get("alertname"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let fingerprint = first_alert
                .get("fingerprint")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return format!("alert:{alertname}:{fingerprint}");
        }

        format!("source:{source_id}:event:{}", Uuid::new_v4())
    }
}

pub struct GithubNormalizer;

impl PayloadNormalizer for GithubNormalizer {
    fn normalize(&self, payload: &Value) -> NormalizedEvent {
        let repo = payload
            .get("repository")
            .and_then(Value::as_object)
            .and_then(|repository| repository.get("full_name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown/repo");

        if let Some(workflow_run) = payload.get("workflow_run").and_then(Value::as_object) {
            let name = workflow_run
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let conclusion = workflow_run
                .get("conclusion")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let head_branch = workflow_run
                .get("head_branch")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let action = payload
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            return NormalizedEvent {
                text: format!(
                    "[GitHub] Workflow: {name}\nAction: {action}\nConclusion: {conclusion}\nBranch: {head_branch}\nRepository: {repo}"
                ),
                title: Some(format!("Workflow {name}")),
                severity: Some(conclusion.to_owned()),
                labels: HashMap::from([
                    ("event".to_owned(), "workflow_run".to_owned()),
                    ("repository".to_owned(), repo.to_owned()),
                ]),
            };
        }

        if payload.get("ref").is_some()
            && payload.get("commits").and_then(Value::as_array).is_some()
        {
            let reference = payload
                .get("ref")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let pusher = payload
                .get("pusher")
                .and_then(Value::as_object)
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            let mut commit_lines = Vec::new();
            if let Some(commits) = payload.get("commits").and_then(Value::as_array) {
                for commit in commits {
                    let id = commit
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let short_id = id.chars().take(7).collect::<String>();
                    let message = commit
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("(no message)");
                    let author = commit
                        .get("author")
                        .and_then(Value::as_object)
                        .and_then(|a| a.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");

                    commit_lines.push(format!("- {short_id} {message} ({author})"));
                }
            }

            let commit_block = if commit_lines.is_empty() {
                "- (no commits)".to_owned()
            } else {
                commit_lines.join("\n")
            };

            return NormalizedEvent {
                text: format!(
                    "[GitHub] Push to {reference}\nRepository: {repo}\nPusher: {pusher}\nCommits:\n{commit_block}"
                ),
                title: Some(format!("Push {reference}")),
                severity: None,
                labels: HashMap::from([
                    ("event".to_owned(), "push".to_owned()),
                    ("repository".to_owned(), repo.to_owned()),
                ]),
            };
        }

        NormalizedEvent {
            text: format!(
                "[GitHub] Event received\nRepository: {repo}\nPayload:\n{}",
                truncate_text(pretty_json(payload), RAW_MAX_BYTES)
            ),
            title: Some("GitHub Event".to_owned()),
            severity: None,
            labels: HashMap::from([("repository".to_owned(), repo.to_owned())]),
        }
    }

    fn derive_scope(&self, payload: &Value, source_id: &str) -> String {
        if let Some(repo) = payload
            .get("repository")
            .and_then(Value::as_object)
            .and_then(|repository| repository.get("full_name"))
            .and_then(Value::as_str)
        {
            return format!("repo:{repo}");
        }

        format!("source:{source_id}:event:{}", Uuid::new_v4())
    }
}

fn pretty_json(payload: &Value) -> String {
    serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
}

fn truncate_text(mut text: String, max_bytes: usize) -> String {
    if text.len() > max_bytes {
        let safe_end = text.floor_char_boundary(max_bytes);
        text.truncate(safe_end);
    }
    text
}

fn object_to_string_map(
    object: Option<&serde_json::Map<String, Value>>,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(object) = object {
        for (key, value) in object {
            let normalized = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            map.insert(key.clone(), normalized);
        }
    }
    map
}

pub fn get_normalizer(format: &str) -> Box<dyn PayloadNormalizer> {
    match format {
        "alertmanager" => Box::new(AlertmanagerNormalizer),
        "github" => Box::new(GithubNormalizer),
        "generic" => Box::new(GenericNormalizer),
        _ => Box::new(RawNormalizer),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn raw_normalizer_pretty_prints_json() {
        let payload = json!({"k":"v","n":1});
        let event = RawNormalizer.normalize(&payload);
        assert!(event.text.contains("\n"));
        assert!(event.text.contains("\"k\": \"v\""));
    }

    #[test]
    fn raw_normalizer_truncates_large_payload() {
        let long = "a".repeat(6000);
        let payload = json!({"body": long});
        let event = RawNormalizer.normalize(&payload);
        assert!(event.text.len() <= RAW_MAX_BYTES);
    }

    #[test]
    fn raw_normalizer_scope_is_unique_per_event() {
        let payload = json!({"k":"v"});
        let first = RawNormalizer.derive_scope(&payload, "github");
        let second = RawNormalizer.derive_scope(&payload, "github");
        assert_ne!(first, second);
        assert!(first.starts_with("source:github:event:"));
    }

    #[test]
    fn generic_normalizer_uses_title_and_body() {
        let payload = json!({
            "title": "Build Failed",
            "body": "CI checks failed",
            "severity": "warning"
        });
        let event = GenericNormalizer.normalize(&payload);
        assert_eq!(event.title.as_deref(), Some("Build Failed"));
        assert_eq!(event.severity.as_deref(), Some("warning"));
        assert_eq!(event.text, "Build Failed\n\nCI checks failed");
    }

    #[test]
    fn generic_normalizer_scope_from_labels() {
        let payload = json!({
            "labels": {
                "team": "platform",
                "service": "gateway"
            }
        });
        let scope = GenericNormalizer.derive_scope(&payload, "webhook");
        assert!(scope.starts_with("source:webhook:event:"));
        // Should include key=value pairs, sorted alphabetically
        assert!(scope.contains("service=gateway"));
        assert!(scope.contains("team=platform"));
    }

    #[test]
    fn generic_normalizer_handles_empty_payload() {
        let payload = json!({});
        let event = GenericNormalizer.normalize(&payload);
        let scope = GenericNormalizer.derive_scope(&payload, "generic");
        assert_eq!(event.text, "{}");
        assert!(scope.starts_with("source:generic:event:"));
    }

    #[test]
    fn alertmanager_normalizer_extracts_single_alert() {
        let payload = json!({
            "status": "firing",
            "alerts": [
                {
                    "status": "firing",
                    "labels": {
                        "alertname": "HighCPU",
                        "severity": "critical",
                        "instance": "api-1"
                    },
                    "annotations": {
                        "summary": "CPU usage is above 90%"
                    },
                    "startsAt": "2026-03-12T10:00:00Z",
                    "fingerprint": "abc123"
                }
            ]
        });
        let event = AlertmanagerNormalizer.normalize(&payload);
        assert!(event.text.contains("[Alertmanager] 🔴 FIRING: HighCPU"));
        assert!(event.text.contains("Severity: critical"));
        assert!(event.text.contains("Instance: api-1"));
        assert!(event.text.contains("Summary: CPU usage is above 90%"));
        assert!(event.text.contains("Started: 2026-03-12T10:00:00Z"));
        assert_eq!(event.severity.as_deref(), Some("critical"));
    }

    #[test]
    fn alertmanager_normalizer_scope_uses_fingerprint() {
        let payload = json!({
            "alerts": [
                {
                    "labels": {"alertname": "DiskFull"},
                    "fingerprint": "fp-001"
                }
            ]
        });
        let scope = AlertmanagerNormalizer.derive_scope(&payload, "alertmanager");
        assert_eq!(scope, "alert:DiskFull:fp-001");
    }

    #[test]
    fn alertmanager_normalizer_handles_multiple_alerts() {
        let payload = json!({
            "status": "firing",
            "alerts": [
                {
                    "status": "firing",
                    "labels": {"alertname": "A", "severity": "warning", "instance": "svc-a"},
                    "annotations": {"summary": "first"},
                    "startsAt": "2026-03-12T10:00:00Z",
                    "fingerprint": "fp-a"
                },
                {
                    "status": "firing",
                    "labels": {"alertname": "B", "severity": "critical", "instance": "svc-b"},
                    "annotations": {"summary": "second"},
                    "startsAt": "2026-03-12T10:01:00Z",
                    "fingerprint": "fp-b"
                }
            ]
        });
        let event = AlertmanagerNormalizer.normalize(&payload);
        assert!(event.text.contains("FIRING: A"));
        assert!(event.text.contains("FIRING: B"));
    }

    #[test]
    fn alertmanager_normalizer_handles_complete_v4_payload() {
        let payload = json!({
            "version": "4",
            "groupKey": "{}:{alertname=\"ServiceDown\"}",
            "truncatedAlerts": 0,
            "status": "firing",
            "receiver": "default",
            "groupLabels": {"alertname": "ServiceDown"},
            "commonLabels": {"severity": "critical", "team": "ops"},
            "commonAnnotations": {"summary": "Service is down"},
            "externalURL": "http://alertmanager.local",
            "alerts": [
                {
                    "status": "firing",
                    "labels": {
                        "alertname": "ServiceDown",
                        "severity": "critical",
                        "instance": "svc-1"
                    },
                    "annotations": {
                        "summary": "Service is down"
                    },
                    "startsAt": "2026-03-12T09:55:00Z",
                    "endsAt": "2026-03-12T10:55:00Z",
                    "generatorURL": "http://prometheus/graph",
                    "fingerprint": "fp-v4"
                }
            ]
        });
        let event = AlertmanagerNormalizer.normalize(&payload);
        assert!(event.text.contains("ServiceDown"));
        assert!(event.labels.contains_key("team"));
        assert!(event.labels.contains_key("severity"));
    }

    #[test]
    fn github_normalizer_push_event_with_commits() {
        let payload = json!({
            "ref": "refs/heads/main",
            "repository": {"full_name": "org/repo"},
            "pusher": {"name": "dragon"},
            "commits": [
                {"id": "abcdef123456", "message": "fix: bug", "author": {"name": "alice"}},
                {"id": "987654fedcba", "message": "feat: add", "author": {"name": "bob"}}
            ]
        });
        let event = GithubNormalizer.normalize(&payload);
        let scope = GithubNormalizer.derive_scope(&payload, "github");
        assert!(event.text.contains("[GitHub] Push to refs/heads/main"));
        assert!(event.text.contains("dragon"));
        assert!(event.text.contains("fix: bug"));
        assert!(event.text.contains("feat: add"));
        assert_eq!(scope, "repo:org/repo");
    }

    #[test]
    fn github_normalizer_workflow_run_event() {
        let payload = json!({
            "action": "completed",
            "repository": {"full_name": "org/repo"},
            "workflow_run": {
                "name": "CI",
                "conclusion": "success",
                "head_branch": "main"
            }
        });
        let event = GithubNormalizer.normalize(&payload);
        assert!(event.text.contains("[GitHub] Workflow"));
        assert!(event.text.contains("CI"));
        assert!(event.text.contains("success"));
    }

    #[test]
    fn get_normalizer_returns_expected_implementations() {
        let payload = json!({
            "title": "T",
            "body": "B",
            "repository": {"full_name": "org/repo"},
            "alerts": [{"labels": {"alertname": "A"}, "fingerprint": "fp"}]
        });

        let generic = get_normalizer("generic").normalize(&payload);
        let github = get_normalizer("github").derive_scope(&payload, "github");
        let alertmanager = get_normalizer("alertmanager").derive_scope(&payload, "am");
        let fallback = get_normalizer("unknown").derive_scope(&payload, "unknown");

        assert_eq!(generic.text, "T\n\nB");
        assert_eq!(github, "repo:org/repo");
        assert_eq!(alertmanager, "alert:A:fp");
        assert!(fallback.starts_with("source:unknown:event:"));
    }

    #[test]
    fn truncate_text_safe_on_multibyte_boundary() {
        // "你好世界" is 4 CJK chars = 12 bytes (3 bytes each)
        let text = "你好世界".to_string();
        assert_eq!(text.len(), 12);

        // Truncate at byte 4 — falls in the middle of "好" (bytes 3..6)
        // Without floor_char_boundary, this would panic
        let result = super::truncate_text(text.clone(), 4);
        // Should truncate to the nearest valid char boundary (byte 3 = end of "你")
        assert_eq!(result, "你");
        assert_eq!(result.len(), 3);

        // Truncate at exact boundary should work fine
        let result = super::truncate_text(text.clone(), 6);
        assert_eq!(result, "你好");

        // Truncate larger than string should return original
        let result = super::truncate_text(text.clone(), 100);
        assert_eq!(result, "你好世界");
    }
}
