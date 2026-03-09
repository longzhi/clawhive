use std::path::Path;

use anyhow::Result;

use clawhive_core::load_config;

pub(crate) fn run(root: &Path) -> Result<()> {
    let config = load_config(&root.join("config"))?;
    println!(
        "Config valid. {} agents, {} providers, {} routing bindings.",
        config.agents.len(),
        config.providers.len(),
        config.routing.bindings.len()
    );
    Ok(())
}
