use anyhow::{anyhow, Result};
use cargo::CargoLocator;
use collections::HashMap;
use dap::DebugAdapterConfig;
use gpui::SharedString;
use locators::DapLocator;

mod cargo;
mod locators;

pub(super) struct LocatorStore {
    locators: HashMap<SharedString, Box<dyn DapLocator>>,
}

impl LocatorStore {
    pub(super) fn new() -> Self {
        let locators = HashMap::from_iter([(
            SharedString::new("cargo"),
            Box::new(CargoLocator {}) as Box<dyn DapLocator>,
        )]);
        Self { locators }
    }

    pub(super) async fn resolve_debug_config(
        &self,
        debug_config: &mut DebugAdapterConfig,
    ) -> Result<()> {
        let Some(locator_name) = &debug_config.locator else {
            log::debug!("Attempted to resolve debug config without a locator field");
            return Ok(());
        };

        match self.locators.get(locator_name as &str) {
            Some(locator) => locator.run_locator(debug_config).await,
            _ => Err(anyhow!("Couldn't find locator {}", locator_name)),
        }
    }
}
