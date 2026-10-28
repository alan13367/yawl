//! The setup wizard's outcome. Every write goes through `ConfigChange`, so
//! setup saves and interactive settings obey the same validation and
//! precedence rules.

use crate::config::{Config, ConfigChange};
use crate::error::Error;

use super::SetupOutcome;

pub(super) fn wizard(config: &Config) -> Result<SetupOutcome, Error> {
    crate::set_interrupted(false);
    let config = crate::tui::run_setup(config)?;
    if config.model.is_some() {
        return Ok(SetupOutcome::Configured(config.into()));
    }
    config.change_global(ConfigChange::SetupSkipped(true))?;
    println!("Setup skipped. Run 'yawl --setup' anytime, or pass --model for one-off use.");
    Ok(SetupOutcome::Skipped)
}
