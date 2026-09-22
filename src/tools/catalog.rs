//! Per-scan caches for the tool registry.
//!
//! The registry rescans on every model step so a tool the model just wrote is
//! usable on its next turn. This cache keeps that promise cheap: filesystem
//! catalogs (skills, subagent presets) are reused while their mtime signature
//! holds, exec tools keep their `--describe` cache, and static entries are
//! shared as `Arc`s instead of being rebuilt.

use std::sync::Arc;

use crate::config::{Config, WebSearchProvider};
use crate::skills::Skill;
use crate::subagent::AgentPreset;
use crate::subagent::presets::DiscoveryCache as PresetsCache;

use super::exec::DescribeCache;
use super::{ToolEntry, files, shell, web};

/// Caches the registry catalogs that would otherwise be rebuilt on every
/// model step: exec `--describe` results, skills, subagent presets, and the
/// static entry sets, which are shared as `Arc`s across scans.
#[derive(Default)]
pub struct CatalogCache {
    describe: DescribeCache,
    skills: crate::skills::DiscoveryCache,
    invocable_skills: Option<InvocableSkills>,
    presets: PresetsCache,
    builtins: Option<BuiltinsCache>,
    subagents: Option<SubagentEntriesCache>,
}

struct InvocableSkills {
    generation: u64,
    skills: Arc<Vec<Skill>>,
}

#[derive(Default)]
struct BuiltinsCache {
    background: bool,
    web_provider: Option<WebSearchProvider>,
    entries: Vec<Arc<ToolEntry>>,
}

struct SubagentEntriesCache {
    generation: u64,
    entries: Vec<Arc<ToolEntry>>,
}

impl CatalogCache {
    pub(super) fn describe(&mut self) -> &mut DescribeCache {
        &mut self.describe
    }

    /// The model-invocable skill catalog, reused while every `SKILL.md` and
    /// source directory keeps its size and modification time.
    pub(super) fn skills(&mut self, config: &Config) -> (Arc<Vec<Skill>>, Vec<String>) {
        let catalog = crate::skills::discover_cached(config, &mut self.skills);
        let generation = self.skills.generation();
        if let Some(cache) = &self.invocable_skills
            && cache.generation == generation
        {
            return (Arc::clone(&cache.skills), catalog.warnings.clone());
        }
        let skills = Arc::new(
            catalog
                .skills
                .iter()
                .filter(|skill| !skill.disable_model_invocation)
                .cloned()
                .collect::<Vec<_>>(),
        );
        self.invocable_skills = Some(InvocableSkills {
            generation,
            skills: Arc::clone(&skills),
        });
        (skills, catalog.warnings.clone())
    }

    /// The builtin entries for the current background/web configuration.
    pub(super) fn builtins(&mut self, config: &Config, background: bool) -> Vec<Arc<ToolEntry>> {
        let web_provider = config.web_browsing.then_some(config.web_search_provider);
        if let Some(cache) = &self.builtins
            && cache.background == background
            && cache.web_provider == web_provider
        {
            return cache.entries.clone();
        }
        let entries = build_builtins(config, background)
            .into_iter()
            .map(Arc::new)
            .collect::<Vec<_>>();
        self.builtins = Some(BuiltinsCache {
            background,
            web_provider,
            entries: entries.clone(),
        });
        entries
    }

    /// Subagent orchestration entries plus the presets they advertise,
    /// reused while the preset files are unchanged.
    pub(super) fn subagent_entries(
        &mut self,
        config: &Config,
    ) -> (Vec<Arc<ToolEntry>>, Vec<AgentPreset>, Vec<String>) {
        let (presets, warnings) =
            crate::subagent::presets::discover_cached(config, &mut self.presets);
        let generation = self.presets.generation();
        if let Some(cache) = &self.subagents
            && cache.generation == generation
        {
            return (cache.entries.clone(), presets, warnings);
        }
        let entries = super::orchestration::entries(&presets)
            .into_iter()
            .map(Arc::new)
            .collect::<Vec<_>>();
        self.subagents = Some(SubagentEntriesCache {
            generation,
            entries: entries.clone(),
        });
        (entries, presets, warnings)
    }
}

fn build_builtins(config: &Config, background: bool) -> Vec<ToolEntry> {
    let mut entries = vec![shell::entry(background)];
    entries.extend(files::builtin_entries());
    if config.web_browsing {
        entries.extend(web::WebTools::entries(config.web_search_provider));
    }
    if background {
        entries.extend(shell::background_entries());
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &std::path::Path) -> Config {
        Config {
            home_dir: root.join("home/.yawl"),
            project_dir: root.join("project/.yawl"),
            ..Config::test_default()
        }
    }

    #[test]
    fn builtin_entries_are_shared_until_the_scan_shape_changes() {
        let root =
            std::env::temp_dir().join(format!("yawl-catalog-builtins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut config = config(&root);
        let mut cache = CatalogCache::default();

        let first = cache.builtins(&config, false);
        let second = cache.builtins(&config, false);
        assert_eq!(first.len(), second.len());
        for (left, right) in first.iter().zip(&second) {
            assert!(
                Arc::ptr_eq(left, right),
                "unchanged scans must share builtin entries"
            );
        }

        config.web_browsing = true;
        let web = cache.builtins(&config, false);
        assert!(web.iter().any(|entry| entry.spec.name == "web_fetch"));
        assert!(!first.iter().any(|entry| entry.spec.name == "web_fetch"));

        let background = cache.builtins(&config, true);
        assert!(
            background
                .iter()
                .any(|entry| entry.spec.name == "shell_output")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn subagent_entries_follow_preset_generations() {
        let root = std::env::temp_dir().join(format!("yawl-catalog-agents-{}", std::process::id()));
        let home = root.join("home/.yawl");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(home.join("agents")).expect("agents dir");
        let config = config(&root);
        let mut cache = CatalogCache::default();

        let (first, presets, warnings) = cache.subagent_entries(&config);
        assert!(warnings.is_empty());
        assert!(presets.iter().any(|preset| preset.name == "scout"));
        let (second, _, _) = cache.subagent_entries(&config);
        for (left, right) in first.iter().zip(&second) {
            assert!(
                Arc::ptr_eq(left, right),
                "unchanged presets must share orchestration entries"
            );
        }

        std::fs::write(
            home.join("agents/reviewer.json"),
            r#"{"description":"reviewer"}"#,
        )
        .expect("preset");
        let (changed, presets, _) = cache.subagent_entries(&config);
        assert!(presets.iter().any(|preset| preset.name == "reviewer"));
        assert_eq!(changed.len(), first.len());
        assert!(
            !Arc::ptr_eq(&changed[0], &first[0]),
            "a changed preset set must rebuild entries"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invocable_skills_are_shared_until_the_catalog_changes() {
        let root = std::env::temp_dir().join(format!("yawl-catalog-skills-{}", std::process::id()));
        let skills = root.join("home/.yawl/skills");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(skills.join("automatic")).expect("skill dir");
        std::fs::write(
            skills.join("automatic/SKILL.md"),
            "---\nname: automatic\ndescription: Use automatically\n---\nRead code.\n",
        )
        .expect("skill");
        let mut config = config(&root);
        let skill_dirs = vec![skills.clone()];
        config.skill_dirs.clone_from(&skill_dirs);
        config.global_skill_dirs = skill_dirs;
        let mut cache = CatalogCache::default();

        let (first, warnings) = cache.skills(&config);
        assert!(warnings.is_empty());
        assert_eq!(first.len(), 1);
        let (second, _) = cache.skills(&config);
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged skills must share the invocable list"
        );

        std::fs::write(
            skills.join("automatic/SKILL.md"),
            "---\nname: automatic\ndescription: Use automatically\ndisable-model-invocation: true\n---\nRead code.\n",
        )
        .expect("manual skill");
        let (filtered, _) = cache.skills(&config);
        assert!(
            filtered.is_empty(),
            "manual-only skills stay out of the catalog"
        );
        assert!(!Arc::ptr_eq(&filtered, &first));
        let _ = std::fs::remove_dir_all(root);
    }
}
