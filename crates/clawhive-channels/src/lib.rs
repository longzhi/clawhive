#[async_trait::async_trait]
pub trait ChannelBot: Send {
    fn channel_type(&self) -> &str;
    fn connector_id(&self) -> &str;
    async fn run(self: Box<Self>) -> anyhow::Result<()>;
}

#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(feature = "discord")]
pub mod discord;

#[cfg(feature = "slack")]
pub mod slack;

#[cfg(feature = "imessage")]
pub mod imessage;
