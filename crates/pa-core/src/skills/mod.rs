//! Skill discovery, validation, and prompt formatting. Port of core/skills.ts.

pub mod diagnostics;
pub mod discovery;
pub mod frontmatter;
pub mod loader;

use std::path::PathBuf;

use serde::Serialize;

/// Max name length per the Agent Skills spec.
pub const MAX_NAME_LENGTH: usize = 64;
/// Max description length per the Agent Skills spec.
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;

pub use diagnostics::{ResourceCollision, ResourceDiagnostic};
pub use discovery::load_skills_from_dir;
pub use loader::{load_skills, LoadSkillsOptions, LoadSkillsResult};

/// Source provenance for a resource (port of source-info.ts, synthetic form).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub path: String,
    pub source: String,
    pub scope: SourceScope,
    pub origin: SourceOrigin,
    pub base_dir: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SourceScope {
    User,
    Project,
    Temporary,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SourceOrigin {
    Package,
    TopLevel,
}

pub fn create_synthetic_source_info(
    path: &str,
    source: &str,
    scope: SourceScope,
    base_dir: Option<&str>,
) -> SourceInfo {
    SourceInfo {
        path: path.to_string(),
        source: source.to_string(),
        scope,
        origin: SourceOrigin::TopLevel,
        base_dir: base_dir.map(str::to_string),
    }
}

/// Python runtime metadata for a Python-backed skill.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SkillPythonMetadata {
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

/// A discovered skill.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source_info: SourceInfo,
    pub disable_model_invocation: bool,
    pub kind: SkillKind,
    pub python: Option<SkillPythonMetadata>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SkillKind {
    Markdown,
    Python,
}

impl Skill {
    pub fn is_python(&self) -> bool {
        self.kind == SkillKind::Python
    }
}

/// Runtime info for kernel-side Python skill preparation.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PythonSkillRuntimeInfo {
    pub name: String,
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

pub fn get_python_skill_runtime_info(skills: &[Skill]) -> Vec<PythonSkillRuntimeInfo> {
    skills
        .iter()
        .filter(|skill| skill.is_python())
        .map(|skill| {
            let python = skill.python.as_ref().expect("python skill has metadata");
            PythonSkillRuntimeInfo {
                name: skill.name.clone(),
                import_name: python.import_name.clone(),
                package_path: python.package_path.clone(),
                pyproject_path: python.pyproject_path.clone(),
            }
        })
        .collect()
}

pub(crate) fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.len() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.is_empty()
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

pub(crate) fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".to_string());
    } else if description.len() > MAX_DESCRIPTION_LENGTH {
        errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.len()
        ));
    }
    errors
}

pub(crate) use validate_description as validate_skill_description;
pub(crate) use validate_name as validate_skill_name;

/// Format skills for a system prompt (Agent Skills XML standard).
/// Skills with disableModelInvocation are excluded.
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use ipython to inspect a skill's file when the task matches its description.".to_string(),
        "Skills with a python_import are prepared in the persistent Python kernel when available and can be called directly by that import name.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <type>{}</type>",
            if skill.is_python() {
                "python"
            } else {
                "markdown"
            }
        ));
        if let Some(python) = &skill.python {
            lines.push(format!(
                "    <python_import>{}</python_import>",
                escape_xml(&python.import_name)
            ));
        }
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.display().to_string())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

pub(crate) fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_rules() {
        assert!(validate_name("web-search", "web-search").is_empty());
        assert!(!validate_name("web_search", "web_search").is_empty());
        assert!(!validate_name("-lead", "-lead").is_empty());
        assert!(!validate_name("a--b", "a--b").is_empty());
        assert!(!validate_name("web", "other").is_empty());
    }

    #[test]
    fn description_validation_rules() {
        assert_eq!(validate_description(""), vec!["description is required"]);
        assert!(validate_description("ok").is_empty());
        let long = "x".repeat(MAX_DESCRIPTION_LENGTH + 1);
        assert!(validate_description(&long).len() == 1);
    }

    #[test]
    fn prompt_formatting_excludes_hidden() {
        let skill = |name: &str, disable: bool| Skill {
            name: name.to_string(),
            description: format!("Does {name}"),
            file_path: PathBuf::from("/s/SKILL.md"),
            base_dir: PathBuf::from("/s"),
            source_info: create_synthetic_source_info("/s", "user", SourceScope::User, None),
            disable_model_invocation: disable,
            kind: SkillKind::Markdown,
            python: None,
        };
        let hidden = format_skills_for_prompt(&[skill("hidden", true)]);
        assert_eq!(hidden, "");
        let formatted = format_skills_for_prompt(&[skill("web-search", false)]);
        assert!(formatted.contains("<name>web-search</name>"));
        assert!(formatted.contains("<type>markdown</type>"));
        assert!(formatted.contains("<description>Does web-search</description>"));
    }
}
