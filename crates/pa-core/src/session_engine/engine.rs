//! SessionEngine assembly: build a running agent session from a config.
//! This is the facade pa-cli/pa-daemon call — the Rust equivalent of the
//! `createAgentSession` wiring: resources, prompt, model, tools, loop, and
//! persistence. The session subscribes persistence listeners on the caller's
//! reactor, so `create_session` is async.

use std::path::PathBuf;
use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::stream::StreamFn;
use pa_agent::types::{Model, ThinkingLevel};

use crate::resources::{load_resources, ResourceLoaderOptions};
use crate::session::manager::SessionManager;
use crate::skills::PromptTemplate;

use super::{AgentSession, PromptOptions, PromptOutcome};

/// Everything needed to assemble a session.
#[derive(Default)]
pub struct SessionEngineConfig {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    /// Resolved model (registry output).
    pub model: Option<Model>,
    /// Thinking level for the session.
    pub thinking_level: Option<ThinkingLevel>,
    /// Provider seam for the loop (required; wire a real provider here).
    pub stream_fn: Option<StreamFn>,
    /// Pre-bridged loop tools (bash/edit/ipython + extensions).
    pub tools: Vec<Arc<dyn pa_agent::types::AgentTool>>,
    /// Override the default system prompt.
    pub custom_system_prompt: Option<String>,
    /// Prompt guideline bullets.
    pub prompt_guidelines: Vec<String>,
    /// Enabled generic MCP server names.
    pub generic_mcp_servers: Vec<String>,
    /// Suppress the rlm recursion guidance.
    pub allow_recursion: Option<bool>,
    /// Session persistence (in-memory when None).
    pub session_manager: Option<SessionManager>,
    /// Extra skill paths.
    pub additional_skill_paths: Vec<String>,
    /// Extra prompt-template paths.
    pub additional_prompt_paths: Vec<String>,
}

/// An assembled, running session.
pub struct SessionEngine {
    pub session: AgentSession,
    pub skills: Vec<crate::skills::Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
    pub agents_files: Vec<crate::resources::ContextFile>,
    pub system_prompt: String,
}

/// Assemble a session: load resources, build the system prompt, and start the
/// loop with persistence wiring.
pub async fn create_session(config: SessionEngineConfig) -> anyhow::Result<SessionEngine> {
    let cwd = config.cwd.clone();
    let resources = load_resources(&ResourceLoaderOptions {
        cwd: cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        additional_skill_paths: config.additional_skill_paths.clone(),
        additional_prompt_paths: config.additional_prompt_paths.clone(),
        no_skills: false,
        no_prompt_templates: false,
        no_context_files: false,
        system_prompt: config.custom_system_prompt.clone(),
        append_system_prompt: Vec::new(),
    });

    let system_prompt = crate::prompts::system_prompt::build_system_prompt(
        &crate::prompts::system_prompt::BuildSystemPromptOptions {
            custom_prompt: resources.system_prompt.clone(),
            cwd: cwd.display().to_string(),
            messages_path: None,
            context_files: resources
                .agents_files
                .iter()
                .map(|file| (file.path.display().to_string(), file.content.clone()))
                .collect(),
            skills: resources.skills.clone(),
            allow_recursion: config.allow_recursion,
            generic_mcp_servers: config.generic_mcp_servers.clone(),
            prompt_guidelines: (config.prompt_guidelines.is_empty())
                .then_some(config.prompt_guidelines.clone()),
            ..Default::default()
        },
    );

    let model = config
        .model
        .ok_or_else(|| anyhow::anyhow!("a resolved model is required"))?;
    let stream_fn = config
        .stream_fn
        .ok_or_else(|| anyhow::anyhow!("a provider stream_fn is required"))?;

    // Session persistence + the runtime wiring: goal/rlm-heartbeat host
    // handlers ride the kernel provisioner, and the agent gains the
    // `ipython` tool backed by that kernel (unless the caller supplied one).
    let session_manager = config
        .session_manager
        .unwrap_or_else(|| SessionManager::in_memory(&cwd));
    let wiring = super::runtime_wiring::wire_session_runtime(session_manager, &config.agent_dir);
    let python_skills = super::runtime_wiring::kernel_python_skills(&resources.skills);
    let session_id = wiring.session.lock().await.get_session_id().to_string();
    let provisioner = super::runtime_wiring::kernel_provisioner(
        session_id,
        wiring.handlers.clone(),
        python_skills,
    );
    let mut tools = config.tools.clone();
    if !tools.iter().any(|tool| tool.name() == "ipython") {
        let definition = crate::tools::ipython::create_ipython_tool_definition(
            &cwd.to_string_lossy(),
            super::runtime_wiring::ipython_tool_options(provisioner),
        );
        tools.push(Arc::new(
            crate::session_engine::tool_bridge::ToolDefinitionBridge::new(definition),
        ));
    }

    let agent = Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some(system_prompt.clone()),
            model: Some(model),
            thinking_level: config.thinking_level,
            tools: Some(tools),
            messages: None,
        },
        stream_fn: Some(stream_fn),
        ..Default::default()
    });

    Ok(SessionEngine {
        session: AgentSession::from_session_arc(
            Arc::new(agent),
            wiring.session.clone(),
            resources.prompts.clone(),
        )
        .await,
        skills: resources.skills,
        prompt_templates: resources.prompts,
        agents_files: resources.agents_files,
        system_prompt,
    })
}

impl SessionEngine {
    /// Prompt the session (delegates to AgentSession::prompt).
    pub async fn prompt(
        &self,
        text: &str,
        options: PromptOptions,
    ) -> anyhow::Result<PromptOutcome> {
        self.session.prompt(text, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_engine::tool_bridge::{bridge_tool, ToolDefinitionBridge};
    use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};
    use pa_agent::scripted::ScriptedProvider;

    fn echo_definition() -> ToolDefinition {
        ToolDefinition {
            name: "echo".to_string(),
            label: "Echo".to_string(),
            description: "Echoes its input".to_string(),
            prompt_snippet: String::new(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            execution_mode: Some(ExecutionMode::Sequential),
            prepare_arguments: None,
            execute: Arc::new(|_id, params, _signal, _on_update| {
                let text = params
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                Box::pin(async move { Ok(ToolExecutionResult::text(format!("echo: {text}"))) })
            }),
        }
    }

    #[tokio::test]
    async fn engine_runs_tool_loop_and_persists() {
        let model = pa_agent::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            cost: Default::default(),
            context_window: 1_000,
            max_tokens: 100,
        };
        let provider = Arc::new(ScriptedProvider::new(model.clone()));
        // First turn: call the tool. Second turn: final text.
        provider.push_tool_call_turn(
            Some("checking"),
            vec![("call-1", "echo", serde_json::json!({ "text": "hi" }))],
        );
        provider.push_text_turn("all done");

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let engine = create_session(SessionEngineConfig {
            cwd: cwd.clone(),
            agent_dir: tmp.path().join("agent"),
            model: Some(model),
            thinking_level: None,
            stream_fn: Some(provider.stream_fn()),
            tools: vec![bridge_tool(echo_definition())],
            custom_system_prompt: None,
            prompt_guidelines: vec![],
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: None,
            additional_skill_paths: vec![],
            additional_prompt_paths: vec![],
        })
        .await
        .unwrap();

        // The system prompt is the default RLM assembly.
        assert!(engine
            .system_prompt
            .starts_with("You are a general purpose agent"));

        let outcome = engine
            .prompt("run the echo tool", PromptOptions::default())
            .await
            .unwrap();
        assert_eq!(outcome, PromptOutcome::Prompt);
        engine.session.agent().wait_for_idle().await;

        // The loop executed the tool and produced the final message.
        let state = engine.session.agent().state().await;
        assert!(state.messages.iter().any(|message| match message {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::ToolResult(
                result,
            )) => {
                result.tool_name == "echo"
            }
            _ => false,
        }));
        assert!(state.messages.iter().any(|message| match message {
            pa_agent::types::AgentMessage::Standard(pa_agent::types::Message::Assistant(
                assistant,
            )) => {
                assistant.content.iter().any(|block| {
                    matches!(
                        block,
                        pa_agent::types::AssistantContent::Text(text) if text.text == "all done"
                    )
                })
            }
            _ => false,
        }));
        // The session persisted user + assistant turns.
        let entries = engine.session.entries().await;
        assert!(entries.iter().any(|entry| matches!(
            entry,
            pa_types::session::FileEntry::Message {
                message: pa_types::session::AgentMessage::User(user),
                ..
            } if user.content.text() == "run the echo tool"
        )));
        let _ = ToolDefinitionBridge::new;
    }
}

#[tokio::test]
async fn create_session_registers_goal_and_heartbeat_handlers() {
    let dir = tempfile::TempDir::new().unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "faux-1".to_string(),
                name: Some("Faux".to_string()),
                reasoning: Some(false),
                input: Some(vec![pa_types::ai::ModelInput::Text]),
                cost: None,
                context_window: Some(100_000),
                max_tokens: Some(4_096),
            }]),
            ..Default::default()
        });
    registration.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "ok",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let model = registration.get_model();
    let agent_model = crate::session_engine::provider_adapter::json_round_trip(&model).unwrap();
    let stream_fn = crate::session_engine::provider_adapter::real_stream_fn(None, model.clone());
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().to_path_buf(),
        model: Some(agent_model),
        stream_fn: Some(stream_fn),
        tools: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    // The agent loop gained the ipython tool backed by the kernel.
    let names: Vec<String> = engine
        .session
        .agent()
        .state()
        .await
        .tools
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert!(
        names.iter().any(|name| name == "ipython"),
        "tools: {names:?}"
    );
}
