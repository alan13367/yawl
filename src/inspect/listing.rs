//! `--list-tools`, `--list-sessions`, and `--list-agents`.

use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use crate::config::Config;
use crate::error::Error;
use crate::tools::{CatalogCache, Registry};

fn safe(text: &str) -> String {
    crate::text::sanitize_line(text, crate::text::Tabs::Space)
}

fn print_json(value: &Value) {
    println!("{}", crate::text::json_line(value));
}

pub(crate) fn tools(config: &Config, machine: bool) {
    let mut cache = CatalogCache::default();
    let registry = Registry::scan_for_main_listing(config, &mut cache);
    let tools = registry.describe_all();
    if machine {
        print_json(&json!({
            "tools": tools.iter().map(|(name, description, origin)| json!({
                "name": name,
                "origin": origin,
                "description": description,
            })).collect::<Vec<_>>(),
            "warnings": registry.warnings,
        }));
        return;
    }
    for (name, description, origin) in tools {
        println!(
            "{}\t{}\n  {}",
            safe(&name),
            safe(&origin),
            safe(&description)
        );
    }
    for warning in registry.warnings {
        eprintln!("warning: skipped {}", safe(&warning));
    }
}

/// Resumable sessions for the working directory, newest first.
pub(crate) fn sessions(config: &Config, machine: bool) -> Result<(), Error> {
    let cwd = crate::config::working_dir();
    let infos = crate::session::list(&config.session_dirs(&cwd).project)?;
    if machine {
        print_json(&json!({
            "sessions": infos.iter().map(|info| json!({
                "id": info.id,
                "model": info.model,
                "modified_unix": info
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_secs()),
                "preview": info.preview,
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    if infos.is_empty() {
        println!("No resumable sessions in this directory.");
    }
    for info in infos {
        println!(
            "{}  {}  {}",
            safe(&info.id),
            safe(&info.model),
            safe(&crate::text::ellipsize(&info.preview, 80))
        );
    }
    Ok(())
}

/// Subagent presets, which `--agent` also accepts.
pub(crate) fn agents(config: &Config, machine: bool) {
    let (presets, warnings) = crate::subagent::presets::discover_cached(
        config,
        &mut crate::subagent::presets::DiscoveryCache::default(),
    );
    if machine {
        print_json(&json!({
            "agents": presets.iter().map(|preset| json!({
                "name": preset.name,
                "description": preset.description,
                "model": preset.model,
                "tools": preset.tools,
            })).collect::<Vec<_>>(),
            "warnings": warnings,
        }));
        return;
    }
    for preset in presets {
        let tools = preset
            .tools
            .as_ref()
            .map_or_else(|| "all tools".to_string(), |tools| tools.join(", "));
        println!(
            "{}\n  {}\n  tools: {}",
            safe(&preset.name),
            safe(&preset.description),
            safe(&tools)
        );
    }
    for warning in warnings {
        eprintln!("warning: {}", safe(&warning));
    }
}
