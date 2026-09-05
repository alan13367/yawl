//! Discovery and expansion of reusable Markdown skill instructions.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::{Config, working_dir};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub directory: PathBuf,
    pub instructions: String,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub skills: Vec<Skill>,
    pub warnings: Vec<String>,
    pub directories: Vec<PathBuf>,
}

/// Scans every active skill source. Global roots are always active. Project
/// roots are added only after the invocation trusts the project.
pub fn discover(config: &Config) -> Catalog {
    discover_from(config, &working_dir())
}

fn discover_from(config: &Config, cwd: &Path) -> Catalog {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut catalog = Catalog::default();
    let mut skills = BTreeMap::new();

    let (global_roots, project_roots) = config.skill_dir_sources();
    for root in global_roots {
        scan_root(root, &mut skills, &mut catalog.warnings);
        catalog.directories.push(root.clone());
    }

    if config.project_skills_trusted {
        if let Some(roots) = project_roots {
            for root in roots {
                scan_root(root, &mut skills, &mut catalog.warnings);
                catalog.directories.push(root.clone());
            }
        }

        let yawl = cwd.join(".yawl/skills");
        scan_root(&yawl, &mut skills, &mut catalog.warnings);
        catalog.directories.push(yawl);

        for root in agent_skill_roots(&cwd) {
            scan_root(&root, &mut skills, &mut catalog.warnings);
            catalog.directories.push(root);
        }
    }

    catalog.directories.dedup();
    catalog.skills = skills.into_values().collect();
    catalog
}

/// Compatibility helper for explicit `/skill:NAME` callers.
pub fn scan(config: &Config) -> Vec<Skill> {
    discover(config).skills
}

pub fn has_project_sources(config: &Config) -> bool {
    if config.has_project_skill_override() {
        return true;
    }
    let cwd = working_dir();
    cwd.join(".yawl/skills").is_dir()
        || agent_skill_roots(&cwd)
            .into_iter()
            .any(|root| root.is_dir())
}

pub fn expand(skill: &Skill, arguments: &str) -> String {
    let mut prompt = format!(
        "Apply the following skill instructions. Resolve relative paths from {}.\n\n<skill name=\"{}\">\n{}\n</skill>",
        skill.directory.display(),
        skill.name,
        skill.instructions.trim()
    );
    if !arguments.trim().is_empty() {
        prompt.push_str("\n\nUser request:\n");
        prompt.push_str(arguments.trim());
    }
    prompt
}

pub(crate) fn tool_result(skill: &Skill) -> String {
    format!(
        "Skill: {}\nLocation: {}\n\n{}",
        skill.name,
        skill.directory.display(),
        skill.instructions.trim()
    )
}

fn agent_skill_roots(cwd: &Path) -> Vec<PathBuf> {
    let boundary = crate::trust::project_root(cwd);
    let mut ancestors = Vec::new();
    for ancestor in cwd.ancestors() {
        ancestors.push(ancestor.to_path_buf());
        if ancestor == boundary {
            break;
        }
    }
    if !ancestors.iter().any(|path| path == &boundary) {
        ancestors.push(boundary);
    }
    ancestors.reverse();
    ancestors
        .into_iter()
        .map(|path| path.join(".agents/skills"))
        .collect()
}

fn scan_root(root: &Path, skills: &mut BTreeMap<String, Skill>, warnings: &mut Vec<String>) {
    let mut candidates = Vec::new();
    let mut visited = HashSet::new();
    collect_candidates(root, root, &mut candidates, &mut visited, warnings);
    candidates.sort();
    for path in candidates {
        match load_skill(&path) {
            Ok(skill) => {
                skills.insert(skill.name.clone(), skill);
            }
            Err(reason) => warnings.push(format!("{}: {reason}", path.display())),
        }
    }
}

fn collect_candidates(
    root: &Path,
    directory: &Path,
    candidates: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    warnings: &mut Vec<String>,
) {
    let canonical = match std::fs::canonicalize(directory) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            warnings.push(format!("{}: {error}", directory.display()));
            return;
        }
    };
    if !visited.insert(canonical) {
        return;
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!("{}: {error}", directory.display()));
            return;
        }
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_candidates(root, &path, candidates, visited, warnings);
        } else if file_type.is_file()
            && (entry.file_name() == "SKILL.md"
                || path.parent() == Some(root)
                    && path.extension().and_then(|value| value.to_str()) == Some("md"))
        {
            candidates.push(path);
        }
    }
}

fn load_skill(path: &Path) -> Result<Skill, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("cannot read: {error}"))?;
    let parsed = frontmatter(&text)?;
    let name = parsed
        .name
        .ok_or_else(|| "frontmatter must define name".to_string())?;
    if !valid_name(&name) {
        return Err("name must contain only ASCII letters, digits, '-' or '_'".into());
    }
    let description = parsed
        .description
        .filter(|description| !description.trim().is_empty())
        .ok_or_else(|| "frontmatter must define a nonempty description".to_string())?;
    if parsed.body.trim().is_empty() {
        return Err("instruction body must not be empty".into());
    }
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let directory = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    Ok(Skill {
        name,
        description,
        path,
        directory,
        instructions: parsed.body.trim().to_string(),
        disable_model_invocation: parsed.disable_model_invocation,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

struct ParsedFrontmatter {
    name: Option<String>,
    description: Option<String>,
    disable_model_invocation: bool,
    body: String,
}

fn frontmatter(text: &str) -> Result<ParsedFrontmatter, String> {
    let normalized = text.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return Err("missing YAML frontmatter".into());
    };
    let Some((header, body)) = rest.split_once("\n---\n") else {
        return Err("unterminated YAML frontmatter".into());
    };
    let mut lines = header.lines().peekable();
    let mut name = None;
    let mut description = None;
    let mut disable_model_invocation = false;
    while let Some(line) = lines.next() {
        if let Some(value) = line.strip_prefix("name:") {
            name = scalar(value);
        } else if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim();
            if matches!(value, ">" | ">-" | "|" | "|-") {
                let separator = if value.starts_with('|') { "\n" } else { " " };
                let mut parts = Vec::new();
                while let Some(line) = lines.next_if(|line| {
                    line.starts_with(' ') || line.starts_with('\t') || line.is_empty()
                }) {
                    let part = line.trim();
                    if !part.is_empty() {
                        parts.push(part);
                    }
                }
                description = (!parts.is_empty()).then(|| parts.join(separator));
            } else {
                description = scalar(value);
            }
        } else if let Some(value) = line.strip_prefix("disable-model-invocation:") {
            disable_model_invocation = match value.trim() {
                "true" => true,
                "false" => false,
                _ => return Err("disable-model-invocation must be true or false".into()),
            };
        }
    }
    Ok(ParsedFrontmatter {
        name,
        description,
        disable_model_invocation,
        body: body.to_string(),
    })
}

fn scalar(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches(|character| matches!(character, '\'' | '"'));
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
    }

    fn test_config(root: &Path) -> Config {
        let skill_dirs = vec![root.join("global")];
        Config {
            home_dir: root.join("home/.yawl"),
            project_dir: root.join("project/.yawl"),
            skill_dirs: skill_dirs.clone(),
            global_skill_dirs: skill_dirs,
            ..Config::test_default()
        }
    }

    #[test]
    fn parses_body_and_manual_only_flag() {
        let parsed = frontmatter(
            "---\nname: review\ndescription: Review code\ndisable-model-invocation: true\n---\nDo it.",
        )
        .unwrap();
        assert_eq!(parsed.name.as_deref(), Some("review"));
        assert_eq!(parsed.description.as_deref(), Some("Review code"));
        assert!(parsed.disable_model_invocation);
        assert_eq!(parsed.body, "Do it.");
    }

    #[test]
    fn discovers_nested_skill_files_and_top_level_markdown() {
        let root = std::env::temp_dir().join(format!("yawl-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("global/nested/review")).unwrap();
        std::fs::write(
            root.join("global/nested/review/SKILL.md"),
            skill("review", "Review changes", "Check the patch."),
        )
        .unwrap();
        std::fs::write(
            root.join("global/format.md"),
            skill("format", "Format code", "Run the formatter."),
        )
        .unwrap();

        let catalog = discover(&test_config(&root));
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
        assert_eq!(
            catalog
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["format", "review"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_missing_descriptions_and_does_not_follow_symlinks() {
        let root = std::env::temp_dir().join(format!("yawl-skills-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("global/bad")).unwrap();
        std::fs::write(
            root.join("global/bad/SKILL.md"),
            "---\nname: bad\n---\nInstructions",
        )
        .unwrap();
        std::os::unix::fs::symlink(&root, root.join("global/loop")).unwrap();

        let catalog = discover(&test_config(&root));
        assert!(catalog.skills.is_empty());
        assert_eq!(catalog.warnings.len(), 1);
        assert!(catalog.warnings[0].contains("nonempty description"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn later_project_sources_override_global_skills_only_when_trusted() {
        let root = std::env::temp_dir().join(format!("yawl-skills-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("global/review")).unwrap();
        std::fs::create_dir_all(root.join("project-override/review")).unwrap();
        std::fs::write(
            root.join("global/review/SKILL.md"),
            skill("review", "Global review", "global"),
        )
        .unwrap();
        std::fs::write(
            root.join("project-override/review/SKILL.md"),
            skill("review", "Project review", "project"),
        )
        .unwrap();
        let mut config = test_config(&root);
        let project_skill_dirs = vec![root.join("project-override")];
        config.skill_dirs.clone_from(&project_skill_dirs);
        config.project_skill_dirs = Some(project_skill_dirs);

        assert_eq!(discover(&config).skills[0].instructions, "global");
        config.project_skills_trusted = true;
        assert_eq!(discover(&config).skills[0].instructions, "project");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn programmatic_skill_directory_replacement_is_discovered() {
        let root =
            std::env::temp_dir().join(format!("yawl-skills-programmatic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("replacement/review")).unwrap();
        std::fs::write(
            root.join("replacement/review/SKILL.md"),
            skill("review", "Replacement review", "replacement"),
        )
        .unwrap();
        let mut config = test_config(&root);
        config.skill_dirs = vec![root.join("replacement")];

        let catalog = discover(&config);

        assert_eq!(catalog.skills[0].instructions, "replacement");
        assert_eq!(catalog.directories, [root.join("replacement")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nearer_agent_directory_overrides_git_root_and_global_skills() {
        let root =
            std::env::temp_dir().join(format!("yawl-skills-ancestors-{}", std::process::id()));
        let repo = root.join("repo");
        let nested = repo.join("packages/app");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".agents/skills/review")).unwrap();
        std::fs::create_dir_all(nested.join(".agents/skills/review")).unwrap();
        std::fs::create_dir_all(root.join("global/review")).unwrap();
        std::fs::write(
            root.join("global/review/SKILL.md"),
            skill("review", "Global", "global"),
        )
        .unwrap();
        std::fs::write(
            repo.join(".agents/skills/review/SKILL.md"),
            skill("review", "Root", "root"),
        )
        .unwrap();
        std::fs::write(
            nested.join(".agents/skills/review/SKILL.md"),
            skill("review", "Nearest", "nearest"),
        )
        .unwrap();
        let mut config = test_config(&root);
        config.project_skills_trusted = true;

        let catalog = discover_from(&config, &nested);
        assert_eq!(catalog.skills[0].instructions, "nearest");
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        assert!(
            catalog
                .directories
                .contains(&canonical_root.join("repo/.agents/skills"))
        );
        assert!(
            catalog
                .directories
                .contains(&canonical_root.join("repo/packages/app/.agents/skills"))
        );
        assert!(
            !catalog
                .directories
                .contains(&canonical_root.join(".agents/skills"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manual_only_skills_remain_in_the_explicit_catalog() {
        let root = std::env::temp_dir().join(format!("yawl-skills-manual-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("global/manual")).unwrap();
        std::fs::write(
            root.join("global/manual/SKILL.md"),
            "---\nname: manual\ndescription: Run on request\ndisable-model-invocation: true\n---\nManual instructions.\n",
        )
        .unwrap();

        let skills = scan(&test_config(&root));
        assert_eq!(skills.len(), 1);
        assert!(skills[0].disable_model_invocation);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn expansion_uses_body_and_source_directory() {
        let skill = Skill {
            name: "review".into(),
            description: "Review code".into(),
            path: Path::new("/tmp/review/SKILL.md").into(),
            directory: Path::new("/tmp/review").into(),
            instructions: "Check the patch.".into(),
            disable_model_invocation: false,
        };
        let prompt = expand(&skill, "focus on safety");
        assert!(prompt.contains("Check the patch."));
        assert!(prompt.contains("/tmp/review"));
        assert!(prompt.contains("focus on safety"));
        assert!(!prompt.contains("description:"));
    }
}
