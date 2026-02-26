use anyhow::Result;
use clawhive_bus::EventBus;

#[tokio::main]
async fn main() -> Result<()> {
    let bus = EventBus::new(256);
    clawhive_tui::run_tui(&bus).await
}
