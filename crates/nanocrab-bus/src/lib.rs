use anyhow::Result;
use nanocrab_schema::Event;
use tokio::sync::mpsc;

pub struct EventBus {
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        Self { tx, rx }
    }

    pub fn sender(&self) -> mpsc::Sender<Event> {
        self.tx.clone()
    }

    pub async fn publish(&self, event: Event) -> Result<()> {
        self.tx.send(event).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
