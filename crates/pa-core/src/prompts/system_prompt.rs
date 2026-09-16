//! System prompt construction. Port of core/system-prompt.ts — model-facing
//! parity: section order and strings must match the TS output.

use std::collections::HashSet;

use super::{
    build_child_agent_doctrine, build_rlm_prompt, build_subagent_guidance,
    ChildAgentDoctrineOptions, RlmPromptOptions, SubagentGuidanceOptions,
};
use crate::skills::{format_skills_for_prompt, get_python_skill_runtime_info, Skill};

pub const REFINE_SKILL_NAME: &str = "refine";

#[derive(Debug, Default)]
pub struct BuildSystemPromptOptions<'a> {
    /// Custom system prompt (replaces the default RLM prompt).
    pub custom_prompt: Option<String>,
    /// Active tools. Tool schemas carry tool descriptions outside the prompt.
    pub selected_tools: Option<Vec<&'a str>>,
    /// Additional guideline bullets appended to the system prompt.
    pub prompt_guidelines: Option<Vec<String>>,
    /// Text to append to the system prompt.
    pub append_system_prompt: Option<String>,
    /// Working directory.
    pub cwd: String,
    /// Conversation log path.
    pub messages_path: Option<String>,
    /// Pre-loaded context files (path, content).
    pub context_files: Vec<(String, String)>,
    /// Pre-loaded skills.
    pub skills: Vec<Skill>,
    /// Whether to include the model-facing rlm recursion guidance.
    pub allow_recursion: Option<bool>,
    /// Fixed recursive-agent depth for this session.
    pub rlm_depth: Option<u32>,
    /// Human-readable parent name or id for child communication doctrine.
    pub rlm_parent_agent: Option<&'a str>,
    /// Enabled user-configured generic MCP servers.
    pub generic_mcp_servers: Vec<String>,
}

fn today() -> String {
    // UTC date in YYYY-MM-DD form; the prompt is date context only.
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or(0);
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_prompt_guidelines(guidelines: &[String]) -> String {
    let mut seen = HashSet::new();
    let mut list = Vec::new();
    for guideline in guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() && seen.insert(normalized.to_string()) {
            list.push(format!("- {normalized}"));
        }
    }
    list.join("\n")
}

fn format_generic_mcp_guidance(servers: &[String]) -> String {
    let mut enabled: Vec<&String> = Vec::new();
    let mut seen = HashSet::new();
    for server in servers {
        if seen.insert(server) {
            enabled.push(server);
        }
    }
    enabled.sort();
    if enabled.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "# Generic MCP Connections".to_string(),
        String::new(),
        "Generic MCP connections are accessed through the pre-imported Python `mcp` object in the Python REPL, not as top-level native tool namespaces or installed Python skills.".to_string(),
        format!(
            "Enabled generic MCP servers: {}.",
            enabled
                .iter()
                .map(|server| format!("`{server}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    for server in &enabled {
        lines.push(format!(
            "For `{server}`, first discover its tools with `await mcp.list_tools(\"{server}\")`, then call one with `await mcp.call_tool(\"{server}\", \"<tool>\", arguments)`."
        ));
    }
    lines.join("\n")
}

/// Build the system prompt with tools, guidelines, and context.
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    let prompt_cwd = options.cwd.replace('\\', "/");
    let prompt_messages_path = options
        .messages_path
        .clone()
        .unwrap_or_else(|| "not persisted".to_string())
        .replace('\\', "/");
    let date = today();
    let append_section = options
        .append_system_prompt
        .as_deref()
        .map(|text| format!("\n\n{text}"))
        .unwrap_or_default();

    let context_files = &options.context_files;
    let tools: Vec<&str> = options
        .selected_tools
        .clone()
        .unwrap_or_else(|| vec!["ipython"]);
    let has_ipython = tools.contains(&"ipython");
    let visible_skills: Vec<&Skill> = options
        .skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    let runtime_info = get_python_skill_runtime_info(
        &visible_skills
            .iter()
            .map(|skill| (*skill).clone())
            .collect::<Vec<_>>(),
    );
    let visible_python_import_names: Vec<&str> = runtime_info
        .iter()
        .map(|info| info.import_name.as_str())
        .collect();
    let has_refine_skill = visible_skills
        .iter()
        .any(|skill| skill.name == REFINE_SKILL_NAME);
    let generic_mcp_section = if has_ipython {
        format_generic_mcp_guidance(&options.generic_mcp_servers)
    } else {
        String::new()
    };

    if let Some(custom_prompt) = &options.custom_prompt {
        let mut prompt = custom_prompt.clone();
        if !context_files.is_empty() {
            prompt.push_str("\n\n# Project Context\n\n");
            prompt.push_str("Project-specific instructions and guidelines:\n\n");
            for (file_path, content) in context_files {
                prompt.push_str(&format!("## {file_path}\n\n{content}\n\n"));
            }
        }
        let custom_prompt_has_file_access = options
            .selected_tools
            .as_ref()
            .is_none_or(|selected| selected.contains(&"ipython") || selected.contains(&"bash"));
        if custom_prompt_has_file_access && !options.skills.is_empty() {
            prompt.push_str(&format_skills_for_prompt(&options.skills));
        }
        prompt.push_str(&format!("\nCurrent date: {date}"));
        prompt.push_str(&format!("\nCurrent working directory: {prompt_cwd}"));
        let doctrine = build_child_agent_doctrine(&ChildAgentDoctrineOptions {
            depth: options.rlm_depth,
            parent_agent: options.rlm_parent_agent,
            installed_skills: visible_python_import_names.clone(),
            active_tools: Some(tools.clone()),
        });
        if let Some(doctrine) = doctrine {
            prompt.push_str(&format!("\n\n{doctrine}"));
        }
        if !generic_mcp_section.is_empty() {
            prompt.push_str(&format!("\n\n{generic_mcp_section}"));
        }
        if !append_section.is_empty() {
            prompt.push_str(&append_section);
        }
        return prompt;
    }

    let mut prompt = build_rlm_prompt(&RlmPromptOptions {
        cwd: prompt_cwd,
        messages_path: prompt_messages_path,
        installed_skills: visible_python_import_names.clone(),
        active_tools: Some(
            tools
                .iter()
                .copied()
                .filter(|name| matches!(*name, "ipython" | "bash" | "edit"))
                .collect(),
        ),
        allow_recursion: options.allow_recursion,
        depth: options.rlm_depth,
        parent_agent: options.rlm_parent_agent,
        ..Default::default()
    });

    let allow_recursion = options.allow_recursion.unwrap_or(true);
    if allow_recursion && has_ipython {
        let visible_names: HashSet<&str> = visible_python_import_names.iter().copied().collect();
        prompt.push_str(&format!(
            "\n\n{}",
            build_subagent_guidance(&SubagentGuidanceOptions {
                include_refine_examples: Some(has_refine_skill),
                has_agent_message: visible_names.contains("agent_message"),
                has_agent_observe: visible_names.contains("agent_observe"),
            })
        ));
    }
    if !generic_mcp_section.is_empty() {
        prompt.push_str(&format!("\n\n{generic_mcp_section}"));
    }
    let guidelines = options
        .prompt_guidelines
        .as_deref()
        .map(format_prompt_guidelines)
        .unwrap_or_default();
    if !guidelines.is_empty() {
        prompt.push_str(&format!("\n\n# Additional Guidance\n\n{guidelines}"));
    }
    if !context_files.is_empty() {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for (file_path, content) in context_files {
            prompt.push_str(&format!("## {file_path}\n\n{content}\n\n"));
        }
    }
    let has_file_access = tools.contains(&"ipython") || tools.contains(&"bash");
    if has_file_access && !options.skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(&options.skills));
    }
    if !append_section.is_empty() {
        prompt.push_str(&append_section);
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{create_synthetic_source_info, SkillKind, SourceScope};
    use std::path::PathBuf;

    fn skill(name: &str, import: Option<&str>) -> Skill {
        Skill {
            name: name.to_string(),
            description: format!("Skill {name}"),
            file_path: PathBuf::from("/skills/SKILL.md"),
            base_dir: PathBuf::from("/skills"),
            source_info: create_synthetic_source_info("/skills", "user", SourceScope::User, None),
            disable_model_invocation: false,
            kind: if import.is_some() {
                SkillKind::Python
            } else {
                SkillKind::Markdown
            },
            python: import.map(|import_name| crate::skills::SkillPythonMetadata {
                import_name: import_name.to_string(),
                package_path: PathBuf::from("/skills"),
                pyproject_path: PathBuf::from("/skills/pyproject.toml"),
            }),
        }
    }

    fn base_options() -> BuildSystemPromptOptions<'static> {
        BuildSystemPromptOptions {
            cwd: "/w".to_string(),
            messages_path: Some("/log.jsonl".to_string()),
            skills: vec![
                skill("web-search", Some("websearch")),
                skill("refine", Some("refine")),
                skill("agent-message", Some("agent_message")),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn default_prompt_composes_rlm_and_skills() {
        let prompt = build_system_prompt(&base_options());
        assert!(prompt.starts_with("You are a general purpose agent"));
        assert!(prompt.contains("# Delegating to sub-agents"));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<python_import>websearch</python_import>"));
        // Python imports flow into the RLM prompt's installed skills list.
        assert!(prompt.contains("`websearch`, `refine`, `agent_message`"));
        // Generic MCP section absent when no servers configured.
        assert!(!prompt.contains("# Generic MCP Connections"));
    }

    #[test]
    fn custom_prompt_appends_context_and_date() {
        let mut options = base_options();
        options.custom_prompt = Some("Be terse.".to_string());
        options.context_files = vec![("AGENTS.md".to_string(), "Rule one.".to_string())];
        let prompt = build_system_prompt(&options);
        assert!(prompt.starts_with("Be terse."));
        assert!(prompt.contains("# Project Context"));
        assert!(prompt.contains("## AGENTS.md\n\nRule one.\n\n"));
        assert!(prompt.contains("\nCurrent date: "));
        assert!(prompt.contains("\nCurrent working directory: /w"));
        // Custom prompts skip the default RLM assembly.
        assert!(!prompt.contains("# Delegating to sub-agents"));
    }

    #[test]
    fn mcp_and_guidelines_sections() {
        let mut options = base_options();
        options.generic_mcp_servers = vec!["t".to_string(), "t".to_string()];
        options.prompt_guidelines = Some(vec!["be careful".to_string(), "be careful".to_string()]);
        let prompt = build_system_prompt(&options);
        assert!(prompt.contains("# Generic MCP Connections"));
        assert!(prompt.contains("Enabled generic MCP servers: `t`."));
        assert!(prompt.contains("# Additional Guidance\n\n- be careful"));
    }
}
