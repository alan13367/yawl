//! Fixtures shared by the TUI test modules.

use std::path::Path;

use crate::agent::Agent;
use crate::config::Config;

/// An agent on model `test` with a fresh session in `root/project` and its
/// global and project config under `root`.
pub(super) fn agent_in(root: &Path) -> Agent {
    let config = Config {
        model: Some("test".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let session = crate::session::Session::create(&config.session_dirs(&cwd).project, &cwd, "test")
        .expect("test session should be created");
    Agent::new(config, "test".into(), session)
}
