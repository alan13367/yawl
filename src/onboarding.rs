//! First-run provider and model setup.

mod discovery;
mod flow;
pub(crate) mod provider;
mod select;
mod terminal;

use crate::config::Config;
use crate::error::Error;

/// Result of the setup wizard.
pub enum SetupOutcome {
    /// A default model is configured. Carries the reloaded config.
    Configured(Box<Config>),
    /// The user skipped setup. A marker was saved so a bare `yawl` run will
    /// not prompt again; setup stays available through `yawl --setup`.
    Skipped,
}

/// Runs the interactive setup wizard.
///
/// # Errors
///
/// Returns an error if terminal input fails, a validated change is rejected
/// at save time, provider discovery fails unexpectedly, or the user presses
/// Ctrl+C.
pub fn run(config: &Config) -> Result<SetupOutcome, Error> {
    flow::wizard(config)
}
