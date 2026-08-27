//! Invocation-level project skill trust resolution and terminal prompting.

use std::io::{self, Write};
use std::path::Path;

use yawl::config::Config;

pub(super) fn resolve(
    config: &mut Config,
    trust_override: bool,
    stdin_is_terminal: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if trust_override {
        config.set_project_skills_trusted(true);
        return Ok(());
    }
    let root = yawl::trust::project_root(&yawl::config::working_dir());
    match yawl::trust::load(&config.home_dir, &root)? {
        yawl::trust::Decision::Trusted => config.set_project_skills_trusted(true),
        yawl::trust::Decision::Denied => config.set_project_skills_trusted(false),
        yawl::trust::Decision::Unknown if !yawl::skills::has_project_sources(config) => {
            config.set_project_skills_trusted(false);
        }
        yawl::trust::Decision::Unknown if stdin_is_terminal => match prompt_project_trust(&root)? {
            ProjectTrustChoice::Permanent => {
                yawl::trust::store(&config.home_dir, &root, true)?;
                config.set_project_skills_trusted(true);
            }
            ProjectTrustChoice::Session => config.set_project_skills_trusted(true),
            ProjectTrustChoice::Deny => {
                yawl::trust::store(&config.home_dir, &root, false)?;
                config.set_project_skills_trusted(false);
            }
        },
        yawl::trust::Decision::Unknown => {
            config.set_project_skills_trusted(false);
            eprintln!(
                "warning: project skill sources under {} are not trusted; pass --trust-project to enable them for this invocation",
                root.display()
            );
        }
    }
    Ok(())
}

enum ProjectTrustChoice {
    Permanent,
    Session,
    Deny,
}

fn prompt_project_trust(root: &Path) -> Result<ProjectTrustChoice, io::Error> {
    loop {
        eprintln!("Project skill sources found under {}.", root.display());
        eprintln!("  1. Trust this project permanently");
        eprintln!("  2. Trust for this session");
        eprintln!("  3. Do not trust");
        eprint!("Choice [3]: ");
        io::stderr().flush()?;
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer)? == 0 {
            return Ok(ProjectTrustChoice::Deny);
        }
        match answer.trim() {
            "1" => return Ok(ProjectTrustChoice::Permanent),
            "2" => return Ok(ProjectTrustChoice::Session),
            "" | "3" => return Ok(ProjectTrustChoice::Deny),
            _ => eprintln!("Enter 1, 2, or 3."),
        }
    }
}
