#[async_trait::async_trait]
pub trait ChannelBot: Send {
    fn channel_type(&self) -> &str;
    fn connector_id(&self) -> &str;
    async fn run(self: Box<Self>) -> anyhow::Result<()>;
}

pub(crate) mod common;

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

#[cfg(feature = "weixin")]
pub mod weixin;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    fn source_path(file_name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join(file_name)
    }

    #[test]
    fn common_module_defines_abort_on_drop() {
        let common = fs::read_to_string(source_path("common.rs"))
            .expect("common.rs should exist for shared channel helpers");

        assert!(common.contains("pub struct AbortOnDrop"));
        assert!(common.contains("impl Drop for AbortOnDrop"));
    }

    #[test]
    fn discord_and_telegram_no_longer_use_hardcoded_300s_timeout() {
        let discord = include_str!("discord.rs");
        let telegram = include_str!("telegram.rs");

        assert!(!discord.contains("Duration::from_secs(300)"));
        assert!(!telegram.contains("Duration::from_secs(300)"));
        assert!(!discord.contains("struct AbortOnDrop"));
        assert!(!telegram.contains("struct AbortOnDrop"));
    }
}
