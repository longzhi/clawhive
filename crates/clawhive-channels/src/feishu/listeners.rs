use std::sync::Arc;

use clawhive_bus::{EventBus, Topic};
use clawhive_schema::{ActionKind, BusMessage};

use super::client::FeishuClient;
use super::message::{
    build_approval_card, build_skill_confirm_card, has_formatting, md_to_feishu_card,
};

pub fn spawn_delivery_listener(
    bus: Arc<EventBus>,
    client: Arc<FeishuClient>,
    connector_id: String,
) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::DeliverAnnounce).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::DeliverAnnounce {
                channel_type,
                connector_id: msg_connector_id,
                conversation_scope,
                text,
            } = msg
            else {
                continue;
            };

            if channel_type != "feishu" || msg_connector_id != connector_id {
                continue;
            }

            let chat_id = conversation_scope.trim_start_matches("chat:");
            let content = serde_json::json!({"text": text}).to_string();

            if let Err(e) = client.send_message(chat_id, "text", &content).await {
                tracing::error!(
                    target: "clawhive::channel::feishu",
                    error = %e,
                    "failed to deliver announce message"
                );
            }
        }
    });
}

pub fn spawn_approval_listener(
    bus: Arc<EventBus>,
    client: Arc<FeishuClient>,
    connector_id: String,
) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::DeliverApprovalRequest).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::DeliverApprovalRequest {
                channel_type,
                connector_id: msg_connector_id,
                conversation_scope,
                short_id,
                agent_id,
                command,
            } = msg
            else {
                continue;
            };

            if channel_type != "feishu" || msg_connector_id != connector_id {
                continue;
            }

            let chat_id = conversation_scope.trim_start_matches("chat:");
            let card = build_approval_card(&agent_id, &command, &short_id);

            if let Err(e) = client.send_card(chat_id, &card).await {
                tracing::error!(
                    target: "clawhive::channel::feishu",
                    error = %e,
                    "failed to send approval card"
                );
            }
        }
    });
}

pub fn spawn_skill_confirm_listener(
    bus: Arc<EventBus>,
    client: Arc<FeishuClient>,
    connector_id: String,
) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::DeliverSkillConfirm).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::DeliverSkillConfirm {
                channel_type,
                connector_id: msg_connector_id,
                conversation_scope,
                token,
                skill_name,
                analysis_text: _,
            } = msg
            else {
                continue;
            };

            if channel_type != "feishu" || msg_connector_id != connector_id {
                continue;
            }

            let chat_id = conversation_scope.trim_start_matches("chat:");
            let card = build_skill_confirm_card(&skill_name, &token);

            if let Err(e) = client.send_card(chat_id, &card).await {
                tracing::error!(
                    target: "clawhive::channel::feishu",
                    error = %e,
                    "failed to send skill confirm card"
                );
            }
        }
    });
}

pub fn spawn_action_listener(bus: Arc<EventBus>, client: Arc<FeishuClient>, connector_id: String) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::ActionReady).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::ActionReady { action } = msg else {
                continue;
            };

            if action.channel_type != "feishu" || action.connector_id != connector_id {
                continue;
            }

            let Some(ref message_id) = action.message_id else {
                continue;
            };

            match action.action {
                ActionKind::Edit { ref new_text } => {
                    let result = if has_formatting(new_text) {
                        let card = md_to_feishu_card(new_text);
                        let content = serde_json::to_string(&card).unwrap_or_default();
                        client
                            .edit_message(message_id, "interactive", &content)
                            .await
                    } else {
                        let content = serde_json::json!({"text": new_text}).to_string();
                        client.edit_message(message_id, "text", &content).await
                    };
                    if let Err(e) = result {
                        tracing::error!(
                            target: "clawhive::channel::feishu",
                            error = %e,
                            "failed to edit message"
                        );
                    }
                }
                ActionKind::Delete => {
                    if let Err(e) = client.delete_message(message_id).await {
                        tracing::error!(
                            target: "clawhive::channel::feishu",
                            error = %e,
                            "failed to delete message"
                        );
                    }
                }
                ActionKind::React { .. } | ActionKind::Unreact { .. } => {
                    tracing::debug!(
                        target: "clawhive::channel::feishu",
                        "feishu does not support message reactions, ignoring"
                    );
                }
            }
        }
    });
}
