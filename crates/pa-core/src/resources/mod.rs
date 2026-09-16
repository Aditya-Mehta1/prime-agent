//! Project context files (AGENTS.md/CLAUDE.md discovery) and the resource
//! loader. Port of core/resource-loader.ts, scoped to the session engine's
//! needs: skills, prompt templates, agents files, and system-prompt sources.
//! Extensions (JS plugins) and package-manager resolution are a seam — they
//! surface as extra skill/prompt paths; themes live in pa-tui.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::skills::diagnostics::ResourceDiagnostic;
use crate::skills::{
    load_prompt_templates, load_skills, LoadPromptTemplatesOptions, LoadSkillsOptions,
    PromptTemplate, Skill,
};

/// AGENTS.md-family candidates, in priority order.
const CONTEXT_FILE_CANDIDATES: [&str; 4] = ["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

/// A loaded context file (path + content).
#[derive(Debug, Clone, PartialEq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    for filename in CONTEXT_FILE_CANDIDATES {
        let file_path = dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&file_path) {
            return Some(ContextFile {
                path: file_path,
                content,
            });
        }
    }
    None
}

/// Load the global (agentDir) context file first, then the nearest-to-root
/// ancestor chain of cwd. Port of loadProjectContextFiles.
pub fn load_project_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut context_files = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    if let Some(global) = load_context_file_from_dir(agent_dir) {
        seen.insert(global.path.clone());
        context_files.push(global);
    }

    // Walk cwd upward, collecting context files; root-most first in output.
    let mut ancestors = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        if let Some(found) = load_context_file_from_dir(&current) {
            if seen.insert(found.path.clone()) {
                ancestors.push(found);
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    ancestors.reverse();
    context_files.extend(ancestors);
    context_files
}

/// Extra resource paths contributed by extensions (loader seam).
#[derive(Debug, Default, Clone)]
pub struct ResourceExtensionPaths {
    pub skill_paths: Vec<String>,
    pub prompt_paths: Vec<String>,
}

/// Loaded resources for a session.
#[derive(Debug, Default)]
pub struct LoadedResources {
    pub skills: Vec<Skill>,
    pub skill_diagnostics: Vec<ResourceDiagnostic>,
    pub prompts: Vec<PromptTemplate>,
    pub agents_files: Vec<ContextFile>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
}

/// Resource loading options (mirrors the TS DefaultResourceLoaderOptions,
/// minus extension/theme machinery).
#[derive(Debug, Default)]
pub struct ResourceLoaderOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub additional_skill_paths: Vec<String>,
    pub additional_prompt_paths: Vec<String>,
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_context_files: bool,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
}

/// Load all session resources from disk. The settings-driven package
/// resolution (npm:/git: sources) is not ported yet; explicit paths and the
/// default user/project directories are.
pub fn load_resources(options: &ResourceLoaderOptions) -> LoadedResources {
    let mut resources = LoadedResources::default();

    // Skills: explicit paths + the default user/project skills dirs (the
    // package-manager resolution step contributes these in TS).
    let default_dirs: Vec<String> = if options.no_skills {
        Vec::new()
    } else {
        vec![
            options
                .agent_dir
                .join("skills")
                .to_string_lossy()
                .to_string(),
            options
                .cwd
                .join(crate::skills::loader::CONFIG_DIR_NAME)
                .join("skills")
                .to_string_lossy()
                .to_string(),
        ]
    };
    let skill_paths = merge_paths(&options.additional_skill_paths, &default_dirs);
    let skills_result = if options.no_skills && skill_paths.is_empty() {
        crate::skills::LoadSkillsResult {
            skills: Vec::new(),
            diagnostics: Vec::new(),
        }
    } else {
        load_skills(&LoadSkillsOptions {
            cwd: options.cwd.clone(),
            agent_dir: options.agent_dir.clone(),
            skill_paths,
            include_defaults: false,
        })
    };
    resources.skills = skills_result.skills;
    resources.skill_diagnostics = skills_result.diagnostics;
    for path in &options.additional_skill_paths {
        if is_local_path(path)
            && !Path::new(path).exists()
            && !resources
                .skill_diagnostics
                .iter()
                .any(|d| diagnostics_path(d) == Some(path.clone()))
        {
            resources.skill_diagnostics.push(ResourceDiagnostic::Error {
                message: "Skill path does not exist".to_string(),
                path: Some(path.clone()),
            });
        }
    }

    // Prompt templates: explicit paths + the default prompt dirs.
    let default_prompt_dirs: Vec<String> = if options.no_prompt_templates {
        Vec::new()
    } else {
        vec![
            options
                .agent_dir
                .join("prompts")
                .to_string_lossy()
                .to_string(),
            options
                .cwd
                .join(crate::skills::loader::CONFIG_DIR_NAME)
                .join("prompts")
                .to_string_lossy()
                .to_string(),
        ]
    };
    let prompt_paths = merge_paths(&options.additional_prompt_paths, &default_prompt_dirs);
    let all_prompts = if options.no_prompt_templates && prompt_paths.is_empty() {
        Vec::new()
    } else {
        load_prompt_templates(&LoadPromptTemplatesOptions {
            cwd: options.cwd.clone(),
            agent_dir: options.agent_dir.clone(),
            prompt_paths,
            include_defaults: false,
        })
    };
    resources.prompts = dedupe_prompts(all_prompts);
    for path in &options.additional_prompt_paths {
        if is_local_path(path) && !Path::new(path).exists() {
            resources.skill_diagnostics.push(ResourceDiagnostic::Error {
                message: "Prompt template path does not exist".to_string(),
                path: Some(path.clone()),
            });
        }
    }

    // Agents files (project context).
    if !options.no_context_files {
        resources.agents_files = load_project_context_files(&options.cwd, &options.agent_dir);
    }

    // System prompt: explicit source or discovered file content.
    let system_source = options.system_prompt.clone().or_else(|| {
        discover_system_prompt_file(&options.cwd, &options.agent_dir)
            .map(|p| p.to_string_lossy().to_string())
    });
    resources.system_prompt = system_source.and_then(|source| resolve_prompt_input(&source));

    // Append system prompt: explicit sources or discovered file.
    let append_sources: Vec<String> = if !options.append_system_prompt.is_empty() {
        options.append_system_prompt.clone()
    } else {
        discover_append_system_prompt_file(&options.cwd, &options.agent_dir)
            .map(|path| path.to_string_lossy().to_string())
            .into_iter()
            .collect()
    };
    resources.append_system_prompt = append_sources
        .iter()
        .filter_map(|source| resolve_prompt_input(source))
        .collect();

    resources
}

fn diagnostics_path(diagnostic: &ResourceDiagnostic) -> Option<String> {
    match diagnostic {
        ResourceDiagnostic::Warning { path, .. } => path.clone(),
        ResourceDiagnostic::Error { path, .. } => path.clone(),
        ResourceDiagnostic::Collision { path, .. } => Some(path.clone()),
    }
}

fn resolve_prompt_input(input: &str) -> Option<String> {
    let path = Path::new(input);
    if path.exists() {
        return std::fs::read_to_string(path).ok();
    }
    Some(input.to_string())
}

fn is_local_path(path: &str) -> bool {
    let trimmed = path.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://"))
}

fn merge_paths(primary: &[String], additional: &[String]) -> Vec<String> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for path in primary.iter().chain(additional) {
        if seen.insert(path.clone()) {
            merged.push(path.clone());
        }
    }
    merged
}

/// Later same-name templates win? No: TS dedupe keeps the FIRST of each name.
fn dedupe_prompts(prompts: Vec<PromptTemplate>) -> Vec<PromptTemplate> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for prompt in prompts {
        if seen.insert(prompt.name.clone()) {
            out.push(prompt);
        }
    }
    out
}

/// cwd/{CONFIG_DIR_NAME}/SYSTEM.md then agentDir/SYSTEM.md.
pub fn discover_system_prompt_file(cwd: &Path, agent_dir: &Path) -> Option<PathBuf> {
    let project = cwd
        .join(crate::skills::loader::CONFIG_DIR_NAME)
        .join("SYSTEM.md");
    if project.exists() {
        return Some(project);
    }
    let global = agent_dir.join("SYSTEM.md");
    if global.exists() {
        return Some(global);
    }
    None
}

/// cwd/{CONFIG_DIR_NAME}/APPEND_SYSTEM.md then agentDir/APPEND_SYSTEM.md.
pub fn discover_append_system_prompt_file(cwd: &Path, agent_dir: &Path) -> Option<PathBuf> {
    let project = cwd
        .join(crate::skills::loader::CONFIG_DIR_NAME)
        .join("APPEND_SYSTEM.md");
    if project.exists() {
        return Some(project);
    }
    let global = agent_dir.join("APPEND_SYSTEM.md");
    if global.exists() {
        return Some(global);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn context_files_global_then_ancestors_root_first() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let deep = tmp.path().join("a").join("b");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&deep).unwrap();
        fs::write(agent_dir.join("AGENTS.md"), "global").unwrap();
        fs::write(tmp.path().join("a").join("AGENTS.md"), "level-a").unwrap();
        fs::write(deep.join("AGENTS.md"), "deep").unwrap();
        let files = load_project_context_files(&deep, &agent_dir);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["global", "level-a", "deep"]);
    }

    #[test]
    fn system_prompt_discovery_prefers_project() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let project = tmp.path().join("proj");
        fs::create_dir_all(agent_dir.join(".prime")).unwrap();
        fs::create_dir_all(project.join(".prime")).unwrap();
        fs::write(agent_dir.join(".prime").join("SYSTEM.md"), "global system").unwrap();
        fs::write(project.join(".prime").join("SYSTEM.md"), "project system").unwrap();
        let found = discover_system_prompt_file(&project, &agent_dir).unwrap();
        assert!(found.to_string_lossy().contains("proj"));
        // Content resolution reads the file.
        assert_eq!(
            resolve_prompt_input(&found.to_string_lossy()),
            Some("project system".to_string())
        );
    }

    #[test]
    fn resources_load_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let cwd = tmp.path().join("proj");
        fs::create_dir_all(agent_dir.join("skills").join("alpha")).unwrap();
        fs::create_dir_all(cwd.join(".prime").join("prompts")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::write(
            agent_dir.join("skills").join("alpha").join("SKILL.md"),
            "---\nname: alpha\ndescription: Alpha skill\n---\nbody",
        )
        .unwrap();
        fs::write(cwd.join(".prime").join("prompts").join("fix.md"), "Fix $1").unwrap();
        fs::write(cwd.join("AGENTS.md"), "Project rules").unwrap();
        fs::write(cwd.join(".prime").join("SYSTEM.md"), "custom system").unwrap();
        let resources = load_resources(&ResourceLoaderOptions {
            cwd: cwd.clone(),
            agent_dir: agent_dir.clone(),
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
            no_skills: false,
            no_prompt_templates: false,
            no_context_files: false,
            system_prompt: None,
            append_system_prompt: vec![],
        });
        assert!(resources.skills.iter().any(|s| s.name == "alpha"));
        assert!(resources.prompts.iter().any(|p| p.name == "fix"));
        assert_eq!(resources.agents_files.len(), 1);
        assert_eq!(resources.agents_files[0].content, "Project rules");
        assert_eq!(resources.system_prompt.as_deref(), Some("custom system"));
        assert!(resources.append_system_prompt.is_empty());
    }
}
