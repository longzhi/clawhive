use anyhow::Result;
use nanocrab_bus::EventBus;

#[tokio::main]
async fn main() -> Result<()> {
    let bus = EventBus::new(256);
    nanocrab_tui::run_tui(&bus).await
}
