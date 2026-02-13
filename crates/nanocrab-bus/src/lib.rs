use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use nanocrab_schema::BusMessage;
use tokio::sync::{mpsc, RwLock};

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum Topic {
    HandleIncomingMessage,
    CancelTask,
    RunScheduledConsolidation,
    MessageAccepted,
    ReplyReady,
    TaskFailed,
    MemoryWriteRequested,
    NeedHumanApproval,
    MemoryReadRequested,
    ConsolidationCompleted,
    StreamDelta,
}

impl Topic {
    pub fn from_message(msg: &BusMessage) -> Self {
        match msg {
            BusMessage::HandleIncomingMessage { .. } => Topic::HandleIncomingMessage,
            BusMessage::CancelTask { .. } => Topic::CancelTask,
            BusMessage::RunScheduledConsolidation => Topic::RunScheduledConsolidation,
            BusMessage::MessageAccepted { .. } => Topic::MessageAccepted,
            BusMessage::ReplyReady { .. } => Topic::ReplyReady,
            BusMessage::TaskFailed { .. } => Topic::TaskFailed,
            BusMessage::MemoryWriteRequested { .. } => Topic::MemoryWriteRequested,
            BusMessage::NeedHumanApproval { .. } => Topic::NeedHumanApproval,
            BusMessage::MemoryReadRequested { .. } => Topic::MemoryReadRequested,
            BusMessage::ConsolidationCompleted { .. } => Topic::ConsolidationCompleted,
            BusMessage::StreamDelta { .. } => Topic::StreamDelta,
        }
    }
}

type Subscriber = mpsc::Sender<BusMessage>;

pub struct EventBus {
    subscribers: Arc<RwLock<HashMap<Topic, Vec<Subscriber>>>>,
    capacity: usize,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            capacity,
        }
    }

    pub async fn subscribe(&self, topic: Topic) -> mpsc::Receiver<BusMessage> {
        let (tx, rx) = mpsc::channel(self.capacity);
        let mut subs = self.subscribers.write().await;
        subs.entry(topic).or_default().push(tx);
        rx
    }

    pub async fn publish(&self, msg: BusMessage) -> Result<()> {
        let topic = Topic::from_message(&msg);
        let subs = self.subscribers.read().await;
        if let Some(subscribers) = subs.get(&topic) {
            for tx in subscribers {
                let _ = tx.try_send(msg.clone());
            }
        }
        Ok(())
    }

    pub fn publisher(&self) -> BusPublisher {
        BusPublisher {
            subscribers: self.subscribers.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BusPublisher {
    subscribers: Arc<RwLock<HashMap<Topic, Vec<Subscriber>>>>,
}

impl BusPublisher {
    pub async fn publish(&self, msg: BusMessage) -> Result<()> {
        let topic = Topic::from_message(&msg);
        let subs = self.subscribers.read().await;
        if let Some(subscribers) = subs.get(&topic) {
            for tx in subscribers {
                let _ = tx.try_send(msg.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nanocrab_schema::OutboundMessage;
    use tokio::time::{timeout, Duration};
    use uuid::Uuid;

    fn reply_ready_message() -> BusMessage {
        BusMessage::ReplyReady {
            outbound: OutboundMessage {
                trace_id: Uuid::new_v4(),
                channel_type: "telegram".to_string(),
                connector_id: "tg_main".to_string(),
                conversation_scope: "chat:123".to_string(),
                text: "reply".to_string(),
                at: Utc::now(),
            },
        }
    }

    #[tokio::test]
    async fn publish_to_no_subscribers_succeeds() {
        let bus = EventBus::new(8);
        let msg = BusMessage::MessageAccepted {
            trace_id: Uuid::new_v4(),
        };

        let result = bus.publish(msg).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn subscribe_and_receive() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe(Topic::ReplyReady).await;
        let msg = reply_ready_message();

        bus.publish(msg).await.unwrap();

        let received = timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(received, BusMessage::ReplyReady { .. }));
    }

    #[tokio::test]
    async fn multiple_subscribers_same_topic() {
        let bus = EventBus::new(8);
        let mut rx1 = bus.subscribe(Topic::ReplyReady).await;
        let mut rx2 = bus.subscribe(Topic::ReplyReady).await;

        bus.publish(reply_ready_message()).await.unwrap();

        let got1 = timeout(Duration::from_millis(100), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let got2 = timeout(Duration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();

        assert!(matches!(got1, BusMessage::ReplyReady { .. }));
        assert!(matches!(got2, BusMessage::ReplyReady { .. }));
    }

    #[tokio::test]
    async fn different_topics_no_crosstalk() {
        let bus = EventBus::new(8);
        let mut reply_rx = bus.subscribe(Topic::ReplyReady).await;

        let msg = BusMessage::TaskFailed {
            trace_id: Uuid::new_v4(),
            error: "test".into(),
        };
        bus.publish(msg).await.unwrap();

        let received = timeout(Duration::from_millis(100), reply_rx.recv()).await;
        assert!(received.is_err());
    }

    #[tokio::test]
    async fn bus_publisher_clone_works() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe(Topic::ReplyReady).await;
        let publisher = bus.publisher();
        let publisher_clone = publisher.clone();

        publisher_clone
            .publish(reply_ready_message())
            .await
            .unwrap();

        let received = timeout(Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(received, BusMessage::ReplyReady { .. }));
    }

    #[tokio::test]
    async fn channel_backpressure_drops_when_full() {
        let bus = EventBus::new(1);
        let mut rx = bus.subscribe(Topic::ReplyReady).await;

        bus.publish(reply_ready_message()).await.unwrap();
        bus.publish(reply_ready_message()).await.unwrap();

        let first = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(first.is_ok());

        let second = timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(second.is_err());
    }

    #[tokio::test]
    async fn topic_from_message_covers_all_variants() {
        let trace_id = Uuid::new_v4();
        let inbound = nanocrab_schema::InboundMessage {
            trace_id,
            channel_type: "telegram".into(),
            connector_id: "tg".into(),
            conversation_scope: "c:1".into(),
            user_scope: "u:1".into(),
            text: "hi".into(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        let cases: Vec<(BusMessage, Topic)> = vec![
            (
                BusMessage::HandleIncomingMessage {
                    inbound: inbound.clone(),
                    resolved_agent_id: "a".into(),
                },
                Topic::HandleIncomingMessage,
            ),
            (BusMessage::CancelTask { trace_id }, Topic::CancelTask),
            (
                BusMessage::RunScheduledConsolidation,
                Topic::RunScheduledConsolidation,
            ),
            (
                BusMessage::MessageAccepted { trace_id },
                Topic::MessageAccepted,
            ),
            (
                BusMessage::ReplyReady {
                    outbound: OutboundMessage {
                        trace_id,
                        channel_type: "t".into(),
                        connector_id: "c".into(),
                        conversation_scope: "s".into(),
                        text: "r".into(),
                        at: Utc::now(),
                    },
                },
                Topic::ReplyReady,
            ),
            (
                BusMessage::TaskFailed {
                    trace_id,
                    error: "e".into(),
                },
                Topic::TaskFailed,
            ),
            (
                BusMessage::MemoryWriteRequested {
                    session_key: "k".into(),
                    speaker: "s".into(),
                    text: "t".into(),
                    importance: 0.5,
                },
                Topic::MemoryWriteRequested,
            ),
            (
                BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: "r".into(),
                },
                Topic::NeedHumanApproval,
            ),
            (
                BusMessage::MemoryReadRequested {
                    session_key: "k".into(),
                    query: "q".into(),
                },
                Topic::MemoryReadRequested,
            ),
            (
                BusMessage::ConsolidationCompleted {
                    concepts_created: 0,
                    concepts_updated: 0,
                    episodes_processed: 0,
                },
                Topic::ConsolidationCompleted,
            ),
            (
                BusMessage::StreamDelta {
                    trace_id,
                    delta: "hello".into(),
                    is_final: false,
                },
                Topic::StreamDelta,
            ),
        ];

        for (msg, expected_topic) in cases {
            assert_eq!(Topic::from_message(&msg), expected_topic);
        }
    }
}
