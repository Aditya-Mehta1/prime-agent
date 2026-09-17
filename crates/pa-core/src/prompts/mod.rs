//! System-prompt fragments for the RLM kernel: the base RLM prompt, child
//! doctrine, and sub-agent delegation guidance. Port of prompts/rlm.ts —
//! strings must stay byte-identical (model-facing parity).

use crate::kernel::bootstrap::default_rlm_extra_import_labels;

#[derive(Debug, Default)]
pub struct RlmPromptOptions<'a> {
    pub cwd: String,
    pub skills_dir: Option<String>,
    pub installed_skills: Vec<&'a str>,
    pub messages_path: String,
    pub allow_recursion: Option<bool>,
    pub depth: Option<u32>,
    pub parent_agent: Option<&'a str>,
    pub active_tools: Option<Vec<&'a str>>,
}

#[derive(Debug, Default)]
pub struct ChildAgentDoctrineOptions<'a> {
    pub depth: Option<u32>,
    pub parent_agent: Option<&'a str>,
    pub installed_skills: Vec<&'a str>,
    pub active_tools: Option<Vec<&'a str>>,
}

const LONG_RUNNING_WORK_PROMPT: &str = "For slow or independently completing work, use a nonblocking control loop: start the work, record its handle or output location, then end your turn. A `bash()` handle left running beyond its creating cell sends a completion follow-up; when it arrives, inspect the saved handle and continue. Reading a finished handle's result first cancels that follow-up.\nWhen delegation is available and useful, assign independent substantive tasks to separate workers. Start independent workers without waiting for each one sequentially, and let them run in parallel.\nDo not keep the turn open by polling with `time.sleep()` or shell `sleep`, and do not replace polling with a long blocking `await`. Await only the short operation needed to start work or inspect a result that is already available; otherwise end the turn.";

const USER_PROGRESS_PROMPT: &str = "As the user-facing root agent, when work follows a plan, uses many subagents, or spans multiple turns, proactively give regular concise progress updates so the user does not have to ask. State the current plan, what has completed, any blockers, the proposed fixes, and the next actions. Lead with user-visible outcomes rather than internal process or gate names. Mention internal details only when they explain a blocker or decision. Send an update at meaningful milestones and before ending a turn while work is still running. Do not repeat unchanged status or interrupt short work with unnecessary updates.";

const SIMPLIFIED_TECHNICAL_ENGLISH_PROMPT: &str = "Use simplified technical English by default for user-facing prose.\nPrefer short sentences, common words, and concrete verbs. State one main action or fact per sentence when practical. Use lists for steps or conditions.\nKeep necessary technical terms, names, commands, code, paths, and exact quoted text unchanged. State uncertainty directly.\nTreat this as clarity guidance, not a claim of formal ASD-STE100 compliance. Preserve a user-requested format, tone, terminology, and necessary precision.";

const REPL_CONTROL_PROMPT: &str = "The `ipython` tool is a persistent Python REPL — the agent's long-lived control environment for reasoning, context management, state, tool orchestration, and recursive subcalls. Top-level `await` works directly. Use it to keep intermediate variables, inspect and transform outputs, and write small helper functions. Compaction removes individual variables whose serialized form exceeds 16 MiB; keep large source data on disk and reload it when needed.\n\nPython is the orchestration language: use Python for loops, conditionals, parsing, and state. Use `bash()` to invoke programs, not to write shell programs — no shell loops or heredocs; do those in Python.\n\nDo not assume the REPL is the native runtime of the external thing being investigated. A repository, package, service, dataset, paper, website, benchmark, or API may have its own environment and normal interface. Evaluate external systems through their own interface, then use the REPL to coordinate the process and analyze what comes back.\n\n`bash(command)` starts a shell command in the background and returns a handle immediately: `h = bash('npm test')`. Use `h.pid` / `h.running` for liveness, `h.tail(n)` / `h.output()` for combined stdout+stderr so far, `h.poll()` for a non-blocking result, `h.kill()` to terminate (SIGTERM, escalating to SIGKILL; on Windows kill() uses taskkill /T and detached or reparented descendants may survive), and `await h` (or `await bash('cmd')`) for the completed result with exit_code, output, and duration. Prefer bash() for long-running commands so the turn keeps working. Run shell commands with `bash()`, not `subprocess`/`os.system`: subprocess calls block the kernel, show the user nothing while they run, and spawn processes the harness cannot see or stop.\n\nImportant: do not install dependencies into the kernel just to make an external project import or run there. If a project import, test, script, CLI, or dependency check is needed, run it through that project's own environment and normal command interface. For example, in a Python repo use its documented commands, `uv run ...`, `.venv/bin/python ...`, or the active project interpreter from the repo root. Treat failures from that native environment as the relevant result.\n\nUse Python for reading, searching, and editing files — it gives you reusable variables you can slice, filter, and act on without re-reading. Always assign read/search results to named variables so you can revisit them later.\n\nEach `bash()` call is its own process, so shell state does not persist between calls; use `os.chdir(...)` for the working directory and `os.environ[...]` for environment variables — both persist in the REPL and apply to later `bash()` calls.\n\nPython state in the kernel persists across cells: named variables, helper functions, classes, imports, notes, parsed outputs, and helper data structures all remain available in every later turn. Tool calls are themselves Python `await` expressions, so their return values can be bound to variables and composed into program logic just like any other call.\n\nContinual harness state is available as `rlm.harness` and `rlm.get_harness_state()`. CRUD calls are local to this Prime Agent session by default: `rlm.harness.create_memory(...)`, `rlm.harness.update_memory(...)`, `rlm.harness.delete_memory(...)`, `rlm.harness.create_skill(...)`, `rlm.harness.update_skill(...)`, `rlm.harness.delete_skill(...)`, `rlm.harness.create_subagent(...)`, `rlm.harness.update_subagent(...)`, `rlm.harness.delete_subagent(...)`, `rlm.harness.create_prompt_note(...)`, `rlm.harness.update_prompt_note(...)`, `rlm.harness.delete_prompt_note(...)`, plus `rlm.harness.record_refinement(...)` and `rlm.harness.overview()`. Use `global_=True` only for stable cross-session lessons; Python reserves `global`, so literal `global=True` is invalid syntax.\n\nTerminology: continual harness names the persisted prompt, memory, skill, and subagent layer; RLM names the runtime, Python REPL kernel, and native call interface exposed to the model.\n\nRLM-native call contract: installed Python skills are pre-imported modules. Read the matching SKILL.md and call its documented function, such as `await <skill_import>.<function>(...)`; when a CLI exists, use `<skill_import> ...` from shell. Continual harness skill entries are Python REPL skills with an explicit Python `reference` and `arguments` contract. Spawn a reusable delegation spec with `await rlm.spawn('sub-task', name='worker')`; admission returns a child handle immediately. Results arrive only through an available messaging capability or files, never as an `rlm.spawn()` return value. Do not invent non-native wrappers such as `call_skill(...)` or `run_subagent(...)`.";

const EDIT_SKILL_LINE: &str = "For targeted existing-file edits, prefer the pre-imported async `edit` skill from the REPL: `old = '''...'''; new = '''...'''; await edit(path=\"pkg/file.py\", old_str=old, new_str=new)`. Use exact old/new strings; if the text contains triple double quotes, use triple single-quoted variables or build `old`/`new` from inspected file slices.";

pub fn build_child_agent_doctrine(options: &ChildAgentDoctrineOptions) -> Option<String> {
    let depth = options.depth.unwrap_or(0);
    let has_ipython = options
        .active_tools
        .as_ref()
        .is_none_or(|tools| tools.contains(&"ipython"));
    let has_agent_message = options.installed_skills.contains(&"agent_message");
    if depth == 0 {
        return None;
    }

    let mut lines = vec![format!(
        "You are a child agent spawned by {}. Task prompts are labeled `[task from parent]`.",
        options.parent_agent.unwrap_or("your parent agent")
    )];
    if has_agent_message && has_ipython {
        lines.push(
            "When a task calls for an answer, reply explicitly with `await agent_message.send(message, receiver_role=\"parent\")`. Not every message or task needs a reply; continue cleanup after sending and go idle normally.".to_string(),
        );
    }
    if has_ipython {
        lines.push(
            "For long-running work, report brief progress with `await rlm.progress_note('...')` (at most 512 characters, throttled to about one note per 10 seconds); the parent sees notes without needing a reply.".to_string(),
        );
    }
    Some(lines.join("\n"))
}

pub fn build_rlm_prompt(options: &RlmPromptOptions) -> String {
    let depth = options.depth.unwrap_or(0);
    let installed_skills = &options.installed_skills;
    let has_agent_message = installed_skills.contains(&"agent_message");
    let has_agent_observe = installed_skills.contains(&"agent_observe");
    let allow_recursion = options.allow_recursion.unwrap_or(true);
    let active_tools = options.active_tools.clone().unwrap_or_default();
    let has_ipython = options
        .active_tools
        .as_ref()
        .is_none_or(|tools| tools.contains(&"ipython"));
    let can_run_shell_skills = has_ipython || active_tools.contains(&"bash");

    let mut parts: Vec<String> = vec![
        "You are a general purpose agent that uses code to solve tasks.".to_string(),
        "You solve tasks by breaking down problems into sub-tasks, writing and executing code, observing results, and iterating one step at a time.".to_string(),
        "When you are done, stop calling tools and state your final answer.".to_string(),
        String::new(),
        LONG_RUNNING_WORK_PROMPT.to_string(),
        String::new(),
    ];
    if depth == 0 {
        parts.push(USER_PROGRESS_PROMPT.to_string());
        parts.push(String::new());
    }
    parts.push(SIMPLIFIED_TECHNICAL_ENGLISH_PROMPT.to_string());
    parts.push(String::new());
    parts.push(format!("Working directory: {}", options.cwd));
    parts.push(format!("Conversation log: {}", options.messages_path));
    parts.push(format!("Recursive agent depth: {depth}"));
    parts.push(format!(
        "Pre-installed Python packages: {}.",
        default_rlm_extra_import_labels().join(", ")
    ));
    parts.push(
        "Install additional packages with `uv pip install <pkg>` (this is a uv-managed venv with no pip module).".to_string(),
    );

    let doctrine_options = ChildAgentDoctrineOptions {
        depth: Some(depth),
        parent_agent: options.parent_agent,
        installed_skills: installed_skills.clone(),
        active_tools: options.active_tools.clone(),
    };
    if let Some(child_doctrine) = build_child_agent_doctrine(&doctrine_options) {
        parts.push(String::new());
        parts.push(child_doctrine);
    }

    let mut skill_lines: Vec<String> = Vec::new();
    if let Some(skills_dir) = &options.skills_dir {
        skill_lines.push(format!(
            "Local skills live under {skills_dir}. Read their SKILL.md files when helpful."
        ));
    }
    if !installed_skills.is_empty() {
        let installed = installed_skills
            .iter()
            .map(|skill| format!("`{skill}`"))
            .collect::<Vec<_>>()
            .join(", ");
        if has_ipython {
            skill_lines.push(format!(
                "Installed Python skill modules (pre-imported): {installed}."
            ));
            skill_lines.push(
                "Read each skill's SKILL.md for its API. Inspect a module with `help(<skill>)` or `dir(<skill>)`, then inspect a documented callable with `inspect.signature(<skill>.<function>)`.".to_string(),
            );
        } else if can_run_shell_skills {
            skill_lines.push(format!(
                "Installed skills available as shell commands: {installed}."
            ));
        }
        if can_run_shell_skills {
            skill_lines.push(
                "Each skill is also available as a shell command by the same name: `<skill> ...`. Discover its CLI usage with `<skill> --help`.".to_string(),
            );
        }
        if has_ipython && installed_skills.contains(&"edit") {
            skill_lines.push(EDIT_SKILL_LINE.to_string());
        }
    }
    if !skill_lines.is_empty() {
        parts.push(String::new());
        parts.extend(skill_lines);
    }
    if has_agent_message {
        parts.push(
            "Agent messaging is restricted to your parent, siblings, and direct children; roots are siblings, and deeper communication relays through the intermediate child.".to_string(),
        );
    }
    if has_agent_observe {
        parts.push(
            "Agent observation is restricted to your parent, siblings, and direct children; roots are siblings, and deeper inspection relays through the intermediate child.".to_string(),
        );
    }

    if depth == 0 && has_ipython {
        parts.push(String::new());
        parts.push(
            "From a daemon-backed depth-0 session, use `await rlm.create_session('task', name='researcher')` to start a separate top-level session. The call returns after the daemon creates the session and accepts its first prompt. Inline and nested sessions cannot use it. `rlm.spawn(...)` still creates a child.".to_string(),
        );
    }

    if allow_recursion && has_ipython {
        parts.push(String::new());
        parts.push(
            "An `rlm` object is already in your global namespace. `await rlm.spawn('sub-task', name='api-reviewer')` spawns a child and returns immediately after task admission with `rlm_child_id`, `name`, `session_dir`, and `model`; it never waits for or returns the child's answer.".to_string(),
        );
        parts.push(
            "`name` is required: choose a stable child name that is unique among siblings."
                .to_string(),
        );
        parts.push(
            "A child inherits your model. If a different model is explicitly requested, use `await rlm.find_models(...)` and an exact returned selector. An unavailable requested model fails spawn; decide whether to retry or omit `model`. Children also inherit your thinking level; the `thinking` option overrides it with any level the resolved child model supports, and an unsupported level fails spawn.".to_string(),
        );
        parts.push(
            if has_agent_observe {
                "Use `await agent_observe.list_agents()` to discover family, including inactive members, and `await rlm.list_subagents()` to recover direct child handles."
            } else {
                "Use `await rlm.list_subagents()` to recover direct child handles after admission."
            }
            .to_string(),
        );
        if has_agent_message {
            parts.push(
                "Children reply explicitly with `await agent_message.send(message, receiver_role='parent')` when an answer is needed. Replies and follow-ups arrive as ordinary agent messages; not every task requires a reply.".to_string(),
            );
            parts.push(
                "Use `agent_message.send(..., receiver_role='child', receiver_name=child.name)` for follow-ups.".to_string(),
            );
        }
        if has_agent_observe {
            parts.push(
                "Use `agent_observe` to inspect a child's rollout. Observation is restricted to your parent, siblings, and direct children; relay through the intermediate child for deeper descendants.".to_string(),
            );
        } else {
            parts.push(
                "Inspect files a child wrote when you need to collect its work without an observation capability.".to_string(),
            );
        }
        parts.push(
            "Spawn independent children in separate calls and end your turn instead of awaiting completion. Multiple replies may arrive over multiple turns. Delete a direct child explicitly with `await rlm.delete_subagent(child)` when it is no longer needed.".to_string(),
        );
    }

    if has_ipython {
        parts.push(String::new());
        parts.push(REPL_CONTROL_PROMPT.to_string());
        if installed_skills.contains(&"refine") {
            parts.push(String::new());
            parts.push(
                "Treat continual harness refinement as a small, evidence-backed update after observing a repeated failure or reusable tactic: diagnose the issue, update the smallest relevant continual harness component, validate on the next action, then record the outcome. Use `await refine.run()` to turn repeated delegation patterns into reusable subagent specs, repeated procedures into skills, durable facts/preferences into memories, and narrow behavioral policies into prompt addendums. It returns immediately and runs when the current turn ends, so continue working normally after calling it. Do not rewrite the whole continual harness when a focused memory, skill, prompt note, or subagent spec is enough.".to_string(),
            );
        }
    }

    parts.join("\n")
}

/// Supplemental sub-agent delegation guidance appended after the base prompt.
#[derive(Debug, Default)]
pub struct SubagentGuidanceOptions {
    pub include_refine_examples: Option<bool>,
    pub has_agent_message: bool,
    pub has_agent_observe: bool,
}

pub fn build_subagent_guidance(options: &SubagentGuidanceOptions) -> String {
    let mut lines = vec![
        "# Delegating to sub-agents".to_string(),
        String::new(),
        "Spawn independent, self-contained work with `handle = await rlm.spawn('task', name='worker')`. This returns at admission, not completion; keep the handle to stop or inspect the child later.".to_string(),
    ];
    if options.has_agent_message {
        lines.push(
            "Ask for an explicit reply when needed. A child replies with `await agent_message.send(message, receiver_role='parent')`; parent follow-ups use `receiver_role='child'` plus the child's name or id. Not every message needs a reply.".to_string(),
        );
    }
    lines.push("Use `await rlm.list_subagents()` after kernel restart or compaction.".to_string());
    lines.push(
        "Long-running children can report in-flight status with `await rlm.progress_note(...)`; `rlm.list_subagents()` shows each child's activity, latest progress note, and staleness.".to_string(),
    );
    if options.has_agent_observe {
        lines.push("Use `agent_observe` for bounded transcript inspection.".to_string());
    }
    lines.push(
        "Fan-in results with `await rlm.collect(targets, timeout_ms=0)`: it returns typed snapshots of direct children (status, answer preview, error) without steering anyone; an explicit timeout blocks only that call until the children settle or the deadline passes.".to_string(),
    );
    lines.push(
        "Large child outputs belong in files that you read selectively; `collect` snapshots are previews, not full results.".to_string(),
    );
    lines.push(
        "Delegate parallel context-heavy research or independent implementation; do a single known lookup, edit, or command inline.".to_string(),
    );
    if options.include_refine_examples.unwrap_or(true) {
        lines.push(
            "Persist genuinely reusable delegation patterns with `await refine.run()`.".to_string(),
        );
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options() -> RlmPromptOptions<'static> {
        RlmPromptOptions {
            cwd: "/w".to_string(),
            messages_path: "/m.jsonl".to_string(),
            installed_skills: vec!["edit", "refine", "agent_message", "agent_observe"],
            ..Default::default()
        }
    }

    #[test]
    fn root_prompt_shape() {
        let prompt = build_rlm_prompt(&base_options());
        assert!(
            prompt.starts_with("You are a general purpose agent that uses code to solve tasks.")
        );
        assert!(prompt.contains("Working directory: /w"));
        assert!(prompt.contains("Conversation log: /m.jsonl"));
        assert!(prompt.contains("Recursive agent depth: 0"));
        assert!(prompt.contains("yaml (PyYAML), tomli"));
        assert!(prompt.contains(
            "Installed Python skill modules (pre-imported): `edit`, `refine`, `agent_message`, `agent_observe`."
        ));
        assert!(prompt.contains("await rlm.create_session('task', name='researcher')"));
        assert!(prompt.contains("await agent_observe.list_agents()"));
        assert!(prompt.contains("await refine.run()"));
        assert!(!prompt.contains("# Delegating to sub-agents"));
    }

    #[test]
    fn child_prompt_gets_doctrine() {
        let mut options = base_options();
        options.depth = Some(2);
        options.parent_agent = Some("the lead");
        let prompt = build_rlm_prompt(&options);
        assert!(prompt.contains("You are a child agent spawned by the lead."));
        assert!(prompt.contains("await agent_message.send(message, receiver_role=\"parent\")"));
        assert!(prompt.contains("await rlm.progress_note('...')"));
        // Root-only guidance is absent.
        assert!(!prompt.contains("await rlm.create_session"));
    }

    #[test]
    fn child_doctrine_none_at_depth_zero() {
        let options = ChildAgentDoctrineOptions::default();
        assert!(build_child_agent_doctrine(&options).is_none());
    }

    #[test]
    fn no_ipython_shell_skills_only() {
        let mut options = base_options();
        options.active_tools = Some(vec!["bash"]);
        let prompt = build_rlm_prompt(&options);
        assert!(prompt.contains("Installed skills available as shell commands:"));
        assert!(prompt.contains(
            "Each skill is also available as a shell command by the same name: `<skill> ...`. Discover its CLI usage with `<skill> --help`."
        ));
        // No REPL control block without ipython.
        assert!(!prompt.contains("persistent Python REPL"));
    }

    #[test]
    fn subagent_guidance_fan_in() {
        let guidance = build_subagent_guidance(&SubagentGuidanceOptions {
            has_agent_message: true,
            has_agent_observe: true,
            ..Default::default()
        });
        assert!(guidance.starts_with("# Delegating to sub-agents"));
        assert!(guidance.contains("await rlm.collect(targets, timeout_ms=0)"));
        assert!(guidance
            .contains("Persist genuinely reusable delegation patterns with `await refine.run()`."));
    }
}

pub mod system_prompt;
