//! Discovery and expansion of reusable Markdown skill instructions.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub instructions: String,
}

/// Scans configured roots. A skill is either `NAME/SKILL.md` or `NAME.md`.
/// Later roots override earlier roots with the same name.
pub fn scan(config: &Config) -> Vec<Skill> {
    let mut skills = BTreeMap::new();
    for root in &config.skill_dirs {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let candidate = if path.is_dir() {
                let file = path.join("SKILL.md");
                file.is_file()
                    .then_some((entry.file_name().to_string_lossy().into_owned(), file))
            } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
                path.file_stem()
                    .map(|stem| (stem.to_string_lossy().into_owned(), path.clone()))
            } else {
                None
            };
            let Some((fallback_name, file)) = candidate else {
                continue;
            };
            let Ok(instructions) = std::fs::read_to_string(&file) else {
                continue;
            };
            let (metadata_name, description) = frontmatter(&instructions);
            let name = metadata_name.unwrap_or(fallback_name);
            if !valid_name(&name) || instructions.trim().is_empty() {
                continue;
            }
            skills.insert(
                name.clone(),
                Skill {
                    name,
                    description: description.unwrap_or_else(|| "Run this skill".into()),
                    path: file,
                    instructions,
                },
            );
        }
    }
    skills.into_values().collect()
}

pub fn expand(skill: &Skill, arguments: &str) -> String {
    let mut prompt = format!(
        "Apply the following skill instructions.\n\n<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
        skill.name,
        skill.path.display(),
        skill.instructions.trim()
    );
    if !arguments.trim().is_empty() {
        prompt.push_str("\n\nUser request:\n");
        prompt.push_str(arguments.trim());
    }
    prompt
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, None);
    };
    let Some((header, _)) = rest.split_once("\n---") else {
        return (None, None);
    };
    let lines = header.lines().collect::<Vec<_>>();
    let mut name = None;
    let mut description = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some(value) = line.strip_prefix("name:") {
            name = scalar(value);
        } else if let Some(value) = line.strip_prefix("description:") {
            let value = value.trim();
            if matches!(value, ">" | "|") {
                let mut parts = Vec::new();
                while lines.get(index + 1).is_some_and(|line| {
                    line.starts_with(' ') || line.starts_with('\t') || line.is_empty()
                }) {
                    index += 1;
                    let part = lines[index].trim();
                    if !part.is_empty() {
                        parts.push(part);
                    }
                }
                description = (!parts.is_empty()).then(|| parts.join(" "));
            } else {
                description = scalar(value);
            }
        }
        index += 1;
    }
    (name, description)
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

    #[test]
    fn reads_simple_frontmatter() {
        let (name, description) =
            frontmatter("---\nname: review\ndescription: 'Review a change'\n---\n# Instructions");
        assert_eq!(name.as_deref(), Some("review"));
        assert_eq!(description.as_deref(), Some("Review a change"));
    }

    #[test]
    fn reads_folded_frontmatter_description() {
        let (_, description) = frontmatter(
            "---\nname: rust\ndescription: >\n  Rust coding guidance.\n  Use for reviews.\nlicense: MIT\n---\nBody",
        );
        assert_eq!(
            description.as_deref(),
            Some("Rust coding guidance. Use for reviews.")
        );
    }

    #[test]
    fn expansion_includes_arguments() {
        let skill = Skill {
            name: "review".into(),
            description: String::new(),
            path: std::path::Path::new("review/SKILL.md").into(),
            instructions: "Check the patch.".into(),
        };
        let prompt = expand(&skill, "focus on safety");
        assert!(prompt.contains("Check the patch."));
        assert!(prompt.contains("focus on safety"));
    }
}
