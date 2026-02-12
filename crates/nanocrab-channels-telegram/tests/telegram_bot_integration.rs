use nanocrab_channels_telegram::TelegramAdapter;

#[test]
#[ignore]
fn telegram_adapter_external_smoke() {
    let adapter = TelegramAdapter::new("tg_main");
    let inbound = adapter.to_inbound(777, 888, "integration");

    assert_eq!(inbound.channel_type, "telegram");
    assert_eq!(inbound.connector_id, "tg_main");
    assert_eq!(inbound.conversation_scope, "chat:777");
    assert_eq!(inbound.user_scope, "user:888");
}
