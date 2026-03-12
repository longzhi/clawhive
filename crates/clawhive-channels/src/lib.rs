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

#[cfg(feature = "whatsapp")]
pub mod whatsapp;

#[cfg(feature = "feishu")]
pub mod feishu;

#[cfg(feature = "dingtalk")]
pub mod dingtalk;

#[cfg(feature = "wecom")]
pub mod wecom;

#[cfg(feature = "web_console")]
pub mod web_console;

#[cfg(feature = "webhook")]
pub mod webhook;
