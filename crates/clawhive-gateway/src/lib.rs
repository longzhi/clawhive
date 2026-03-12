use std::collections::HashMap as StdHashMap;
use std::sync::Arc;

use anyhow::Result;
use clawhive_bus::{BusPublisher, EventBus, Topic};
use clawhive_core::{ApprovalRegistry, Orchestrator, RoutingConfig};
use clawhive_schema::*;
use tokio::sync::Mutex as TokioMutex;
use uuid::Uuid;

pub mod webhook;

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 30,
            burst: 10,
        }
    }
}

struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: chrono::DateTime<chrono::Utc>,
}

impl TokenBucket {
    fn new(config: &RateLimitConfig) -> Self {
        Self {
            tokens: config.burst as f64,
            max_tokens: config.burst as f64,
            refill_rate: config.requests_per_minute as f64 / 60.0,
            last_refill: chrono::Utc::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = chrono::Utc::now();
        let elapsed = (now - self.last_refill).num_milliseconds() as f64 / 1000.0;
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct RateLimiter {
    buckets: Arc<TokioMutex<StdHashMap<String, TokenBucket>>>,
    config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(TokioMutex::new(StdHashMap::new())),
            config,
        }
    }

    pub async fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(&self.config));
        bucket.try_consume()
    }
}

pub struct Gateway {
    orchestrator: Arc<Orchestrator>,
    routing: RoutingConfig,
    bus: BusPublisher,
    rate_limiter: RateLimiter,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    /// Tracks the last active channel per agent for heartbeat delivery.
    last_active_channels: Arc<TokioMutex<StdHashMap<String, ChannelTarget>>>,
}

/// Channel target info for delivering messages.
#[derive(Debug, Clone)]
pub struct ChannelTarget {
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
}

impl Gateway {
    pub fn new(
        orchestrator: Arc<Orchestrator>,
        routing: RoutingConfig,
        bus: BusPublisher,
        rate_limiter: RateLimiter,
        approval_registry: Option<Arc<ApprovalRegistry>>,
    ) -> Self {
        Self {
            orchestrator,
            routing,
            bus,
            rate_limiter,
            approval_registry,
            last_active_channels: Arc::new(TokioMutex::new(StdHashMap::new())),
        }
    }

    async fn try_handle_approve(&self, inbound: &InboundMessage) -> Option<OutboundMessage> {
        let text = inbound.text.trim();
        if !text.starts_with("/approve") {
            return None;
        }

        let registry = self.approval_registry.as_ref()?;
        let make_reply = |text: String| OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type.clone(),
            connector_id: inbound.connector_id.clone(),
            conversation_scope: inbound.conversation_scope.clone(),
            text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        };

        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() < 3 {
            return Some(make_reply(
                "Usage: /approve <id> allow|always|deny".to_string(),
            ));
        }

        let short_id = parts[1];
        let decision = match parts[2].to_ascii_lowercase().as_str() {
            "allow" | "once" | "allow-once" => ApprovalDecision::AllowOnce,
            "always" | "allow-always" | "always-allow" => ApprovalDecision::AlwaysAllow,
            "deny" | "reject" | "block" => ApprovalDecision::Deny,
            _ => {
                return Some(make_reply(format!(
                    "Unknown decision '{}'. Use: allow, always, or deny",
                    parts[2]
                )));
            }
        };

        match registry
            .resolve_by_short_id(short_id, decision.clone())
            .await
        {
            Ok(()) => Some(make_reply(format!("✅ Approval resolved: {:?}", decision))),
            Err(e) => Some(make_reply(format!("❌ {e}"))),
        }
    }

    pub fn resolve_agent(&self, inbound: &InboundMessage) -> String {
        for binding in &self.routing.bindings {
            if binding.channel_type == inbound.channel_type
                && binding.connector_id == inbound.connector_id
            {
                match binding.match_rule.kind.as_str() {
                    "dm" if !inbound.conversation_scope.contains("group") => {
                        return binding.agent_id.clone();
                    }
                    "mention" if inbound.is_mention => {
                        if let Some(pattern) = &binding.match_rule.pattern {
                            if inbound.mention_target.as_deref() == Some(pattern.as_str()) {
                                return binding.agent_id.clone();
                            }
                        }
                    }
                    "group" => {
                        return binding.agent_id.clone();
                    }
                    _ => {}
                }
            }
        }
        self.routing.default_agent_id.clone()
    }

    pub async fn handle_inbound_for_agent(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<OutboundMessage> {
        let trace_id = inbound.trace_id;

        // Track last active channel per agent (skip heartbeat/system channels)
        if inbound.channel_type != "heartbeat" && inbound.channel_type != "system" {
            let mut channels = self.last_active_channels.lock().await;
            channels.insert(
                agent_id.to_string(),
                ChannelTarget {
                    channel_type: inbound.channel_type.clone(),
                    connector_id: inbound.connector_id.clone(),
                    conversation_scope: inbound.conversation_scope.clone(),
                },
            );
        }

        let _ = self
            .bus
            .publish(BusMessage::HandleIncomingMessage {
                inbound: inbound.clone(),
                resolved_agent_id: agent_id.to_string(),
            })
            .await;

        let _ = self
            .bus
            .publish(BusMessage::MessageAccepted { trace_id })
            .await;

        match self.orchestrator.handle_inbound(inbound, agent_id).await {
            Ok(outbound) => Ok(outbound),
            Err(err) => {
                let _ = self
                    .bus
                    .publish(BusMessage::TaskFailed {
                        trace_id,
                        error: err.to_string(),
                    })
                    .await;
                Err(err)
            }
        }
    }

    pub async fn handle_inbound(&self, inbound: InboundMessage) -> Result<OutboundMessage> {
        if let Some(approval_response) = self.try_handle_approve(&inbound).await {
            return Ok(approval_response);
        }

        if !self.rate_limiter.check(&inbound.user_scope).await {
            return Err(anyhow::anyhow!("rate limited: too many requests"));
        }

        let agent_id = self.resolve_agent(&inbound);
        self.handle_inbound_for_agent(inbound, &agent_id).await
    }

    /// Get the last active channel for an agent (for heartbeat delivery).
    pub async fn last_active_channel(&self, agent_id: &str) -> Option<ChannelTarget> {
        let channels = self.last_active_channels.lock().await;
        channels.get(agent_id).cloned()
    }

    /// Publish a DeliverAnnounce message to the bus.
    pub async fn publish_announce(
        &self,
        channel_type: &str,
        connector_id: &str,
        conversation_scope: &str,
        text: &str,
    ) -> Result<()> {
        self.bus
            .publish(BusMessage::DeliverAnnounce {
                channel_type: channel_type.to_string(),
                connector_id: connector_id.to_string(),
                conversation_scope: conversation_scope.to_string(),
                text: text.to_string(),
            })
            .await
    }
}

pub fn spawn_scheduled_task_listener(
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::ScheduledTaskTriggered).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::ScheduledTaskTriggered {
                schedule_id,
                agent_id,
                payload,
                delivery,
                triggered_at,
            } = msg
            else {
                continue;
            };

            // Spawn each scheduled task in its own tokio task so that a panic
            // in one task does not kill the entire listener loop.
            let gw = Arc::clone(&gateway);
            let task_bus = Arc::clone(&bus);
            let sid = schedule_id.clone();
            let handle = tokio::spawn(async move {
                handle_scheduled_task(
                    gw,
                    task_bus,
                    schedule_id,
                    agent_id,
                    payload,
                    delivery,
                    triggered_at,
                )
                .await;
            });

            // If the spawned task panicked, publish a completion event so the
            // scheduler clears the running_at_ms marker instead of waiting for
            // the 2-hour stuck timeout.
            if let Err(join_err) = handle.await {
                tracing::error!(
                    schedule_id = %sid,
                    error = %join_err,
                    "Scheduled task panicked — publishing error completion"
                );
                let _ = bus
                    .publish(BusMessage::ScheduledTaskCompleted {
                        schedule_id: sid,
                        status: ScheduledRunStatus::Error,
                        error: Some(format!("task panicked: {join_err}")),
                        started_at: triggered_at,
                        ended_at: chrono::Utc::now(),
                        delivery_status: ScheduledDeliveryStatus::NotDelivered,
                        delivery_error: Some("task panicked before delivery".to_string()),
                        response: None,
                    })
                    .await;
            }
        }
    })
}

/// Process a single scheduled task. Extracted so it can be spawned in its own
/// tokio task, isolating panics from the listener loop.
async fn handle_scheduled_task(
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
    schedule_id: String,
    agent_id: String,
    payload: ScheduledTaskPayload,
    delivery: ScheduledDeliveryInfo,
    triggered_at: chrono::DateTime<chrono::Utc>,
) {
    match payload {
        ScheduledTaskPayload::DirectDeliver { text } => {
            let ended_at = chrono::Utc::now();
            let delivery_outcome = deliver_if_needed(
                &bus,
                &delivery,
                &DeliveryAttempt {
                    schedule_id: &schedule_id,
                    status: ScheduledRunStatus::Ok,
                    response: Some(format!("⏰ {}", text)),
                    error: None,
                    started_at: triggered_at,
                    ended_at,
                },
            )
            .await;

            let mut status = ScheduledRunStatus::Ok;
            let mut error = None;
            if matches!(
                delivery_outcome.status,
                ScheduledDeliveryStatus::NotDelivered
            ) && !delivery.best_effort
            {
                status = ScheduledRunStatus::Error;
                error = Some(
                    delivery_outcome
                        .error
                        .clone()
                        .unwrap_or_else(|| "delivery failed".to_string()),
                );
            }

            let _ = bus
                .publish(BusMessage::ScheduledTaskCompleted {
                    schedule_id,
                    status,
                    error,
                    started_at: triggered_at,
                    ended_at,
                    delivery_status: delivery_outcome.status,
                    delivery_error: delivery_outcome.error,
                    response: Some(text),
                })
                .await;
        }
        ScheduledTaskPayload::SystemEvent { text } => {
            let (ch_type, conn_id, conv_scope): (String, String, String) = match (
                &delivery.source_channel_type,
                &delivery.source_connector_id,
                &delivery.source_conversation_scope,
            ) {
                (Some(ct), Some(ci), Some(cs)) => (ct.clone(), ci.clone(), cs.clone()),
                _ => {
                    tracing::warn!(
                        schedule_id = %schedule_id,
                        "SystemEvent missing source scope, falling back to DirectDeliver"
                    );
                    let ended_at = chrono::Utc::now();
                    let delivery_outcome = deliver_if_needed(
                        &bus,
                        &delivery,
                        &DeliveryAttempt {
                            schedule_id: &schedule_id,
                            status: ScheduledRunStatus::Ok,
                            response: Some(format!("⏰ {}", text)),
                            error: None,
                            started_at: triggered_at,
                            ended_at,
                        },
                    )
                    .await;

                    let _ = bus
                        .publish(BusMessage::ScheduledTaskCompleted {
                            schedule_id,
                            status: ScheduledRunStatus::Ok,
                            error: None,
                            started_at: triggered_at,
                            ended_at,
                            delivery_status: delivery_outcome.status,
                            delivery_error: delivery_outcome.error,
                            response: Some(text),
                        })
                        .await;
                    return;
                }
            };

            let user_scope = delivery
                .source_user_scope
                .clone()
                .unwrap_or_else(|| "user:scheduler".into());

            let inbound = InboundMessage {
                trace_id: Uuid::new_v4(),
                channel_type: ch_type,
                connector_id: conn_id,
                conversation_scope: conv_scope,
                user_scope,
                text: format!("[Scheduled Reminder]\n{}", text),
                at: triggered_at,
                thread_id: None,
                is_mention: false,
                mention_target: None,
                message_id: None,
                attachments: vec![],
                group_context: None,
                message_source: None,
            };

            match gateway.handle_inbound_for_agent(inbound, &agent_id).await {
                Ok(outbound) => {
                    let ended_at = chrono::Utc::now();
                    let delivery_outcome = deliver_if_needed(
                        &bus,
                        &delivery,
                        &DeliveryAttempt {
                            schedule_id: &schedule_id,
                            status: ScheduledRunStatus::Ok,
                            response: Some(outbound.text.clone()),
                            error: None,
                            started_at: triggered_at,
                            ended_at,
                        },
                    )
                    .await;

                    let mut status = ScheduledRunStatus::Ok;
                    let mut error = None;
                    if matches!(
                        delivery_outcome.status,
                        ScheduledDeliveryStatus::NotDelivered
                    ) && !delivery.best_effort
                    {
                        status = ScheduledRunStatus::Error;
                        error = Some(
                            delivery_outcome
                                .error
                                .clone()
                                .unwrap_or_else(|| "delivery failed".to_string()),
                        );
                    }

                    let _ = bus
                        .publish(BusMessage::ScheduledTaskCompleted {
                            schedule_id,
                            status,
                            error,
                            started_at: triggered_at,
                            ended_at,
                            delivery_status: delivery_outcome.status,
                            delivery_error: delivery_outcome.error,
                            response: Some(outbound.text),
                        })
                        .await;
                }
                Err(e) => {
                    let ended_at = chrono::Utc::now();
                    let exec_error = e.to_string();
                    let delivery_outcome = deliver_if_needed(
                        &bus,
                        &delivery,
                        &DeliveryAttempt {
                            schedule_id: &schedule_id,
                            status: ScheduledRunStatus::Error,
                            response: None,
                            error: Some(exec_error.clone()),
                            started_at: triggered_at,
                            ended_at,
                        },
                    )
                    .await;

                    let _ = bus
                        .publish(BusMessage::ScheduledTaskCompleted {
                            schedule_id,
                            status: ScheduledRunStatus::Error,
                            error: Some(exec_error),
                            started_at: triggered_at,
                            ended_at,
                            delivery_status: delivery_outcome.status,
                            delivery_error: delivery_outcome.error,
                            response: None,
                        })
                        .await;
                }
            }
        }
        ScheduledTaskPayload::AgentTurn {
            message,
            model: _,
            thinking: _,
            timeout_seconds,
            light_context: _,
        } => {
            // AgentTurn always uses an isolated session so the agent starts
            // with a clean context and cannot be influenced by prior chat history.
            let isolated_scope = format!("schedule:{}:{}", schedule_id, Uuid::new_v4());
            let (channel_type, connector_id, conversation_scope): (String, String, String) = match (
                &delivery.source_channel_type,
                &delivery.source_connector_id,
            ) {
                (Some(ct), Some(ci)) => (ct.clone(), ci.clone(), isolated_scope),
                _ => {
                    tracing::warn!(
                        schedule_id = %schedule_id,
                        "AgentTurn schedule {} has no source channel configured — approval requests will not be routable",
                        schedule_id
                    );
                    ("scheduler".into(), schedule_id.clone(), isolated_scope)
                }
            };

            let user_scope = delivery
                .source_user_scope
                .clone()
                .unwrap_or_else(|| "user:scheduler".into());

            let inbound = InboundMessage {
                trace_id: Uuid::new_v4(),
                channel_type,
                connector_id,
                conversation_scope,
                user_scope,
                text: message,
                at: triggered_at,
                thread_id: None,
                is_mention: false,
                mention_target: None,
                message_id: None,
                attachments: vec![],
                group_context: None,
                message_source: Some("scheduled_task".into()),
            };

            let effective_timeout = timeout_seconds.clamp(30, 3600);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(effective_timeout),
                gateway.handle_inbound_for_agent(inbound, &agent_id),
            )
            .await;

            match result {
                Ok(Ok(outbound)) => {
                    let ended_at = chrono::Utc::now();
                    let delivery_outcome = deliver_if_needed(
                        &bus,
                        &delivery,
                        &DeliveryAttempt {
                            schedule_id: &schedule_id,
                            status: ScheduledRunStatus::Ok,
                            response: Some(outbound.text.clone()),
                            error: None,
                            started_at: triggered_at,
                            ended_at,
                        },
                    )
                    .await;

                    let mut status = ScheduledRunStatus::Ok;
                    let mut error = None;
                    if matches!(
                        delivery_outcome.status,
                        ScheduledDeliveryStatus::NotDelivered
                    ) && !delivery.best_effort
                    {
                        status = ScheduledRunStatus::Error;
                        error = Some(
                            delivery_outcome
                                .error
                                .clone()
                                .unwrap_or_else(|| "delivery failed".to_string()),
                        );
                    }

                    let _ = bus
                        .publish(BusMessage::ScheduledTaskCompleted {
                            schedule_id,
                            status,
                            error,
                            started_at: triggered_at,
                            ended_at,
                            delivery_status: delivery_outcome.status,
                            delivery_error: delivery_outcome.error,
                            response: Some(outbound.text),
                        })
                        .await;
                }
                Ok(Err(e)) => {
                    let ended_at = chrono::Utc::now();
                    let exec_error = e.to_string();
                    let delivery_outcome = deliver_if_needed(
                        &bus,
                        &delivery,
                        &DeliveryAttempt {
                            schedule_id: &schedule_id,
                            status: ScheduledRunStatus::Error,
                            response: None,
                            error: Some(exec_error.clone()),
                            started_at: triggered_at,
                            ended_at,
                        },
                    )
                    .await;

                    let _ = bus
                        .publish(BusMessage::ScheduledTaskCompleted {
                            schedule_id,
                            status: ScheduledRunStatus::Error,
                            error: Some(exec_error),
                            started_at: triggered_at,
                            ended_at,
                            delivery_status: delivery_outcome.status,
                            delivery_error: delivery_outcome.error,
                            response: None,
                        })
                        .await;
                }
                Err(_) => {
                    let ended_at = chrono::Utc::now();
                    let timeout_error = format!("execution timed out after {}s", effective_timeout);
                    let delivery_outcome = deliver_if_needed(
                        &bus,
                        &delivery,
                        &DeliveryAttempt {
                            schedule_id: &schedule_id,
                            status: ScheduledRunStatus::Error,
                            response: None,
                            error: Some(timeout_error.clone()),
                            started_at: triggered_at,
                            ended_at,
                        },
                    )
                    .await;

                    let _ = bus
                        .publish(BusMessage::ScheduledTaskCompleted {
                            schedule_id,
                            status: ScheduledRunStatus::Error,
                            error: Some(timeout_error),
                            started_at: triggered_at,
                            ended_at,
                            delivery_status: delivery_outcome.status,
                            delivery_error: delivery_outcome.error,
                            response: None,
                        })
                        .await;
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct DeliveryOutcome {
    status: ScheduledDeliveryStatus,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct DeliveryAttempt<'a> {
    schedule_id: &'a str,
    status: ScheduledRunStatus,
    response: Option<String>,
    error: Option<String>,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: chrono::DateTime<chrono::Utc>,
}

async fn notify_failure_destination(
    bus: &Arc<EventBus>,
    delivery: &ScheduledDeliveryInfo,
    schedule_id: &str,
    reason: &str,
) {
    let Some(dest) = &delivery.failure_destination else {
        return;
    };
    let (Some(channel_type), Some(connector_id), Some(conversation_scope)) =
        (&dest.channel, &dest.connector_id, &dest.conversation_scope)
    else {
        return;
    };

    let _ = bus
        .publish(BusMessage::DeliverAnnounce {
            channel_type: channel_type.clone(),
            connector_id: connector_id.clone(),
            conversation_scope: conversation_scope.clone(),
            text: format!("⚠️ Schedule '{schedule_id}' delivery failed: {reason}"),
        })
        .await;
}

async fn deliver_if_needed(
    bus: &Arc<EventBus>,
    delivery: &ScheduledDeliveryInfo,
    attempt: &DeliveryAttempt<'_>,
) -> DeliveryOutcome {
    match delivery.mode {
        ScheduledDeliveryMode::None => DeliveryOutcome {
            status: ScheduledDeliveryStatus::NotRequested,
            error: None,
        },
        ScheduledDeliveryMode::Announce => {
            let text = match (attempt.response.as_deref(), attempt.error.as_deref()) {
                (Some(r), _) if !r.trim().is_empty() => r.to_string(),
                (_, Some(e)) if !e.trim().is_empty() => format!("❌ {e}"),
                _ => {
                    return DeliveryOutcome {
                        status: ScheduledDeliveryStatus::NotRequested,
                        error: None,
                    };
                }
            };

            let ch = delivery
                .channel
                .as_ref()
                .or(delivery.source_channel_type.as_ref());
            let conn = delivery
                .connector_id
                .as_ref()
                .or(delivery.source_connector_id.as_ref());
            let scope = delivery.source_conversation_scope.as_ref();
            let (Some(ch), Some(conn), Some(scope)) = (ch, conn, scope) else {
                let reason =
                    "announce delivery target incomplete (missing channel/connector/scope)";
                notify_failure_destination(bus, delivery, attempt.schedule_id, reason).await;
                return DeliveryOutcome {
                    status: ScheduledDeliveryStatus::NotDelivered,
                    error: Some(reason.to_string()),
                };
            };

            match bus
                .publish(BusMessage::DeliverAnnounce {
                    channel_type: ch.clone(),
                    connector_id: conn.clone(),
                    conversation_scope: scope.clone(),
                    text,
                })
                .await
            {
                Ok(()) => DeliveryOutcome {
                    status: ScheduledDeliveryStatus::Delivered,
                    error: None,
                },
                Err(e) => {
                    let reason = format!("announce delivery publish failed: {e}");
                    notify_failure_destination(bus, delivery, attempt.schedule_id, &reason).await;
                    DeliveryOutcome {
                        status: ScheduledDeliveryStatus::NotDelivered,
                        error: Some(reason),
                    }
                }
            }
        }
        ScheduledDeliveryMode::Webhook => {
            let Some(url) = &delivery.webhook_url else {
                let reason = "webhook delivery mode set but no webhook_url provided".to_string();
                tracing::warn!("{reason}");
                notify_failure_destination(bus, delivery, attempt.schedule_id, &reason).await;
                return DeliveryOutcome {
                    status: ScheduledDeliveryStatus::NotDelivered,
                    error: Some(reason),
                };
            };

            let run_status = match attempt.status {
                ScheduledRunStatus::Ok => "ok",
                ScheduledRunStatus::Error => "error",
                ScheduledRunStatus::Skipped => "skipped",
            };

            let payload = webhook::WebhookPayload {
                schedule_id: attempt.schedule_id.to_string(),
                status: run_status.into(),
                response: attempt.response.clone(),
                error: attempt.error.clone(),
                started_at: attempt.started_at,
                ended_at: attempt.ended_at,
                duration_ms: (attempt.ended_at.timestamp_millis()
                    - attempt.started_at.timestamp_millis())
                .max(0) as u64,
            };

            match webhook::deliver_webhook(url, &payload).await {
                Ok(()) => DeliveryOutcome {
                    status: ScheduledDeliveryStatus::Delivered,
                    error: None,
                },
                Err(e) => {
                    tracing::warn!(url = %url, error = %e, "Webhook delivery failed");
                    let reason = e.to_string();
                    notify_failure_destination(bus, delivery, attempt.schedule_id, &reason).await;
                    DeliveryOutcome {
                        status: ScheduledDeliveryStatus::NotDelivered,
                        error: Some(reason),
                    }
                }
            }
        }
    }
}

pub fn spawn_approval_delivery_listener(bus: Arc<EventBus>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let publisher = bus.publisher();
        let mut rx = bus.subscribe(Topic::NeedHumanApproval).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::NeedHumanApproval {
                trace_id,
                reason: _,
                agent_id,
                command,
                network_target,
                source_channel_type,
                source_connector_id,
                source_conversation_scope,
            } = msg
            else {
                continue;
            };

            let (Some(ch_type), Some(conn_id), Some(conv_scope)) = (
                source_channel_type,
                source_connector_id,
                source_conversation_scope,
            ) else {
                continue;
            };

            let short_id = trace_id.to_string()[..8].to_string();
            let command = if let Some(target) = network_target {
                format!("{command}\nNetwork: {target}")
            } else {
                command
            };

            let _ = publisher
                .publish(BusMessage::DeliverApprovalRequest {
                    channel_type: ch_type,
                    connector_id: conn_id,
                    conversation_scope: conv_scope,
                    short_id,
                    agent_id,
                    command,
                })
                .await;
        }
    })
}

/// Spawns a listener that handles WaitTask completion events.
/// When a wait task completes, the result is delivered to the originating session.
pub fn spawn_wait_task_listener(
    _gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::WaitTaskCompleted).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::WaitTaskCompleted {
                task_id,
                session_key,
                status,
                message,
                output,
            } = msg
            else {
                continue;
            };

            tracing::info!(
                task_id = %task_id,
                session_key = %session_key,
                status = %status,
                "Wait task completed"
            );

            // Parse session_key to extract channel info
            // Session keys follow format: "channel_type:connector_id:conversation_scope"
            // e.g., "telegram:tg_main:chat:12345"
            let parts: Vec<&str> = session_key.splitn(3, ':').collect();
            if parts.len() < 3 {
                tracing::warn!(
                    session_key = %session_key,
                    "Invalid session key format for wait task delivery"
                );
                continue;
            }

            let channel_type = parts[0].to_string();
            let connector_id = parts[1].to_string();
            let conversation_scope = parts[2].to_string();

            // Format the delivery message
            let delivery_text = if let Some(out) = output {
                let output_preview: String = out.chars().take(500).collect();
                format!("{}\n\n```\n{}\n```", message, output_preview)
            } else {
                message
            };

            // Deliver via DeliverAnnounce
            let _ = bus
                .publish(BusMessage::DeliverAnnounce {
                    channel_type,
                    connector_id,
                    conversation_scope,
                    text: delivery_text,
                })
                .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use clawhive_bus::{EventBus, Topic};
    use clawhive_core::*;

    use clawhive_memory::MemoryStore;

    use clawhive_provider::{register_builtin_providers, ProviderRegistry};
    use clawhive_runtime::NativeExecutor;
    use clawhive_scheduler::ScheduleManager;
    use clawhive_schema::{ApprovalDecision, BusMessage, InboundMessage};

    use super::*;

    fn make_gateway() -> (Gateway, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        let router = LlmRouter::new(registry, aliases, vec![]);
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                &tmp.path().join("config/schedules.d"),
                &tmp.path().join("data/schedules"),
                Arc::new(EventBus::new(16)),
            )
            .unwrap(),
        );
        let agents = vec![FullAgentConfig {
            agent_id: "clawhive-main".into(),
            enabled: true,
            security: SecurityMode::default(),
            identity: None,
            model_policy: ModelPolicy {
                primary: "sonnet".into(),
                fallbacks: vec![],
                thinking_level: None,
                context_window: None,
            },
            tool_policy: None,
            memory_policy: None,
            sub_agent: None,
            workspace: None,
            heartbeat: None,
            exec_security: None,
            sandbox: None,
        }];
        let orch = Arc::new(
            OrchestratorBuilder::new(
                router,
                publisher.clone(),
                memory,
                Arc::new(NativeExecutor),
                tmp.path().to_path_buf(),
                schedule_manager,
            )
            .agents(agents)
            .build(),
        );
        let routing = RoutingConfig {
            default_agent_id: "clawhive-main".into(),
            bindings: vec![],
        };
        let rate_limiter = RateLimiter::new(RateLimitConfig::default());
        (
            Gateway::new(orch, routing, publisher, rate_limiter, None),
            tmp,
        )
    }

    async fn make_gateway_with_receivers() -> (
        Gateway,
        tokio::sync::mpsc::Receiver<BusMessage>,
        tokio::sync::mpsc::Receiver<BusMessage>,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        let router = LlmRouter::new(registry, aliases, vec![]);
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = EventBus::new(16);
        let handle_incoming_rx = bus.subscribe(Topic::HandleIncomingMessage).await;
        let message_accepted_rx = bus.subscribe(Topic::MessageAccepted).await;
        let publisher = bus.publisher();
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                &tmp.path().join("config/schedules.d"),
                &tmp.path().join("data/schedules"),
                Arc::new(EventBus::new(16)),
            )
            .unwrap(),
        );
        let agents = vec![FullAgentConfig {
            agent_id: "clawhive-main".into(),
            enabled: true,
            security: SecurityMode::default(),
            identity: None,
            model_policy: ModelPolicy {
                primary: "sonnet".into(),
                fallbacks: vec![],
                thinking_level: None,
                context_window: None,
            },
            tool_policy: None,
            memory_policy: None,
            sub_agent: None,
            workspace: None,
            heartbeat: None,
            exec_security: None,
            sandbox: None,
        }];
        let orch = Arc::new(
            OrchestratorBuilder::new(
                router,
                publisher.clone(),
                memory,
                Arc::new(NativeExecutor),
                tmp.path().to_path_buf(),
                schedule_manager,
            )
            .agents(agents)
            .build(),
        );
        let routing = RoutingConfig {
            default_agent_id: "clawhive-main".into(),
            bindings: vec![],
        };
        let rate_limiter = RateLimiter::new(RateLimitConfig::default());
        (
            Gateway::new(orch, routing, publisher, rate_limiter, None),
            handle_incoming_rx,
            message_accepted_rx,
            tmp,
        )
    }

    #[tokio::test]
    async fn gateway_e2e_inbound_to_outbound() {
        let (gw, _tmp) = make_gateway();
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:100".into(),
            user_scope: "user:200".into(),
            text: "ping".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        let out = gw.handle_inbound(inbound).await.unwrap();
        assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
    }

    #[tokio::test]
    async fn resolve_agent_default() {
        let (gw, _tmp) = make_gateway();
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "test".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "clawhive-main");
    }

    #[tokio::test]
    async fn resolve_agent_mention_binding() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "mention".into(),
                pattern: Some("@mybot".into()),
            },
            agent_id: "clawhive-builder".into(),
            delivery: None,
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "@mybot hello".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: true,
            mention_target: Some("@mybot".into()),
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "clawhive-builder");
    }

    #[tokio::test]
    async fn rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 5,
        });
        for _ in 0..5 {
            assert!(limiter.check("user:1").await);
        }
    }

    #[tokio::test]
    async fn rate_limiter_blocks_after_burst() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 2,
        });
        assert!(limiter.check("user:1").await);
        assert!(limiter.check("user:1").await);
        assert!(!limiter.check("user:1").await);
    }

    #[tokio::test]
    async fn rate_limiter_different_users_independent() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 1,
        });
        assert!(limiter.check("user:1").await);
        assert!(limiter.check("user:2").await);
        assert!(!limiter.check("user:1").await);
    }

    #[tokio::test]
    async fn resolve_agent_dm_binding() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "dm".into(),
                pattern: None,
            },
            agent_id: "clawhive-dm".into(),
            delivery: None,
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:private_1".into(),
            user_scope: "user:1".into(),
            text: "dm msg".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "clawhive-dm");
    }

    #[tokio::test]
    async fn resolve_agent_dm_binding_skips_group() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "dm".into(),
                pattern: None,
            },
            agent_id: "clawhive-dm".into(),
            delivery: None,
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "group:chat:123".into(),
            user_scope: "user:1".into(),
            text: "group msg".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "clawhive-main");
    }

    #[tokio::test]
    async fn resolve_agent_group_binding() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "group".into(),
                pattern: None,
            },
            agent_id: "clawhive-group".into(),
            delivery: None,
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:999".into(),
            user_scope: "user:1".into(),
            text: "any msg".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "clawhive-group");
    }

    #[tokio::test]
    async fn handle_inbound_rejects_when_rate_limited() {
        let (mut gw, _tmp) = make_gateway();
        gw.rate_limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 1,
        });
        let make_inbound = || InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:ratelimit_test".into(),
            text: "ping".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };

        let first = gw.handle_inbound(make_inbound()).await;
        assert!(first.is_ok());

        let second = gw.handle_inbound(make_inbound()).await;
        assert!(second.is_err());
        assert!(second.unwrap_err().to_string().contains("rate limited"));
    }

    #[tokio::test]
    async fn handle_inbound_publishes_handle_incoming_before_accept() {
        let (gw, mut incoming_rx, mut accepted_rx, _tmp) = make_gateway_with_receivers().await;
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:pubtest".into(),
            user_scope: "user:pubtest".into(),
            text: "ping".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };

        let expected_trace = inbound.trace_id;
        let expected_conv = inbound.conversation_scope.clone();
        let expected_user = inbound.user_scope.clone();

        let _ = gw.handle_inbound(inbound).await.unwrap();

        let incoming =
            tokio::time::timeout(std::time::Duration::from_millis(200), incoming_rx.recv())
                .await
                .unwrap()
                .unwrap();
        match incoming {
            BusMessage::HandleIncomingMessage {
                inbound,
                resolved_agent_id,
            } => {
                assert_eq!(inbound.trace_id, expected_trace);
                assert_eq!(inbound.conversation_scope, expected_conv);
                assert_eq!(inbound.user_scope, expected_user);
                assert_eq!(resolved_agent_id, "clawhive-main");
            }
            _ => panic!("expected HandleIncomingMessage event"),
        }

        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(200), accepted_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            accepted,
            BusMessage::MessageAccepted { trace_id } if trace_id == expected_trace
        ));
    }

    #[tokio::test]
    async fn approve_command_resolves_pending_by_short_id() {
        let (mut gw, _tmp) = make_gateway();
        let approval_registry = Arc::new(ApprovalRegistry::new());
        gw.approval_registry = Some(approval_registry.clone());

        let trace_id = uuid::Uuid::new_v4();
        let short_id = trace_id.to_string()[..8].to_string();
        let rx = approval_registry
            .request(trace_id, "echo ok".to_string(), "agent-x".to_string())
            .await;

        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:approve".into(),
            user_scope: "user:approve".into(),
            text: format!("/approve {short_id} allow"),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };

        let out = gw.handle_inbound(inbound).await.unwrap();
        assert!(out.text.contains("Approval resolved"));
        let decision = rx.await.unwrap();
        assert_eq!(decision, ApprovalDecision::AllowOnce);
    }

    #[tokio::test]
    async fn approve_command_with_invalid_args_returns_usage() {
        let (mut gw, _tmp) = make_gateway();
        gw.approval_registry = Some(Arc::new(ApprovalRegistry::new()));

        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:approve".into(),
            user_scope: "user:approve".into(),
            text: "/approve".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };

        let out = gw.handle_inbound(inbound).await.unwrap();
        assert!(out.text.contains("Usage: /approve"));
    }

    #[test]
    fn rate_limit_config_default_values() {
        let config = RateLimitConfig::default();
        assert_eq!(config.requests_per_minute, 30);
        assert_eq!(config.burst, 10);
    }

    #[tokio::test]
    async fn resolve_agent_mismatched_connector_uses_default() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_other".into(),
            match_rule: MatchRule {
                kind: "dm".into(),
                pattern: None,
            },
            agent_id: "clawhive-other".into(),
            delivery: None,
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "test".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
            message_source: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "clawhive-main");
    }

    #[tokio::test]
    async fn agent_turn_uses_delivery_source_for_approval() {
        let (gw, _tmp) = make_gateway();
        let sched_bus = Arc::new(EventBus::new(16));
        let mut completed_rx = sched_bus.subscribe(Topic::ScheduledTaskCompleted).await;

        let _handle = spawn_scheduled_task_listener(Arc::new(gw), sched_bus.clone());
        // Yield to let the spawned listener subscribe before we publish.
        tokio::task::yield_now().await;

        sched_bus
            .publish(BusMessage::ScheduledTaskTriggered {
                schedule_id: "sched-approval-test".into(),
                agent_id: "clawhive-main".into(),
                payload: clawhive_schema::ScheduledTaskPayload::AgentTurn {
                    message: "/model".into(),
                    model: None,
                    thinking: None,
                    timeout_seconds: 30,
                    light_context: false,
                },
                delivery: clawhive_schema::ScheduledDeliveryInfo {
                    mode: clawhive_schema::ScheduledDeliveryMode::None,
                    channel: None,
                    connector_id: None,
                    source_channel_type: Some("telegram".into()),
                    source_connector_id: Some("tg_main".into()),
                    source_conversation_scope: Some("chat:tg_123".into()),
                    source_user_scope: Some("user:tg_user".into()),
                    webhook_url: None,
                    failure_destination: None,
                    best_effort: true,
                },
                triggered_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), completed_rx.recv())
            .await
            .expect("timed out waiting for ScheduledTaskCompleted")
            .expect("channel closed");

        if let BusMessage::ScheduledTaskCompleted { response, .. } = msg {
            let resp = response.expect("expected a response from /model command");
            assert!(
                resp.contains("telegram:tg_main"),
                "Expected telegram channel in session key, got: {resp}"
            );
            assert!(
                !resp.contains("scheduler:"),
                "Should not contain scheduler channel, got: {resp}"
            );
        } else {
            panic!("Expected BusMessage::ScheduledTaskCompleted");
        }
    }

    #[tokio::test]
    async fn agent_turn_fallback_when_no_delivery_source() {
        let (gw, _tmp) = make_gateway();
        let sched_bus = Arc::new(EventBus::new(16));
        let mut completed_rx = sched_bus.subscribe(Topic::ScheduledTaskCompleted).await;

        let _handle = spawn_scheduled_task_listener(Arc::new(gw), sched_bus.clone());
        // Yield to let the spawned listener subscribe before we publish.
        tokio::task::yield_now().await;

        sched_bus
            .publish(BusMessage::ScheduledTaskTriggered {
                schedule_id: "sched-fallback-test".into(),
                agent_id: "clawhive-main".into(),
                payload: clawhive_schema::ScheduledTaskPayload::AgentTurn {
                    message: "/model".into(),
                    model: None,
                    thinking: None,
                    timeout_seconds: 30,
                    light_context: false,
                },
                delivery: clawhive_schema::ScheduledDeliveryInfo {
                    mode: clawhive_schema::ScheduledDeliveryMode::None,
                    channel: None,
                    connector_id: None,
                    source_channel_type: None,
                    source_connector_id: None,
                    source_conversation_scope: None,
                    source_user_scope: None,
                    webhook_url: None,
                    failure_destination: None,
                    best_effort: true,
                },
                triggered_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), completed_rx.recv())
            .await
            .expect("timed out waiting for ScheduledTaskCompleted")
            .expect("channel closed");

        if let BusMessage::ScheduledTaskCompleted {
            status, response, ..
        } = msg
        {
            // Task completed without panicking — fallback worked
            assert!(
                matches!(status, clawhive_schema::ScheduledRunStatus::Ok),
                "Expected Ok status with fallback behavior"
            );
            let resp = response.expect("expected a response from /model command");
            assert!(
                resp.contains("scheduler:sched-fallback-test"),
                "Expected scheduler fallback channel in session key, got: {resp}"
            );
        } else {
            panic!("Expected BusMessage::ScheduledTaskCompleted");
        }
    }
}
