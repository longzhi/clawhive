use std::sync::Arc;

use anyhow::Result;
use clawhive_bus::EventBus;
use clawhive_core::approval::ApprovalRegistry;
use clawhive_gateway::Gateway;

pub mod code;
pub mod dashboard;
pub mod shared;

pub use dashboard::BusReceivers;

pub async fn run_tui(
    bus: &EventBus,
    approval_registry: Option<Arc<ApprovalRegistry>>,
) -> Result<()> {
    dashboard::run_tui(bus, approval_registry).await
}

pub async fn subscribe_all(bus: &EventBus) -> BusReceivers {
    dashboard::subscribe_all(bus).await
}

pub async fn run_tui_from_receivers(
    receivers: BusReceivers,
    approval_registry: Option<Arc<ApprovalRegistry>>,
) -> Result<()> {
    dashboard::run_tui_from_receivers(receivers, approval_registry).await
}

pub async fn run_code_tui(
    bus: &EventBus,
    gateway: Arc<Gateway>,
    approval_registry: Option<Arc<ApprovalRegistry>>,
) -> Result<()> {
    code::run(bus, gateway, approval_registry).await
}
