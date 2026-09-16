# Model-facing surface contract

Ground truth for the RLM-1 post-training surface. Derived from the live TS product instance.

## System prompt structure

The base system prompt given to the model contains, in order:

1. Identity block ("You are a general purpose agent...", working dir, conversation log path,
   recursive agent depth, pre-installed Python packages, installed skill modules).
2. Tool-call contract: installed Python skills are pre-imported modules; read SKILL.md;
   call documented functions; CLI fallback; continual-harness entries are Python REPL skills
   with explicit `reference`/`arguments`.
3. REPL kernel rules: persistent Python state across cells, `bash()` background-handle
   contract (h.pid, h.running, h.tail, h.output, h.poll, h.kill, await), no subprocess/os.system,
   os.chdir/environ persistence, "no long blocking awaits/polling".
4. Delegation contract: `await rlm.spawn('sub-task', name=...)` returns immediately with
   rlm_child_id/name/session_dir/model; results only via `agent_message.send` replies or files;
   `rlm.list_subagents()`, `rlm.collect(targets, timeout_ms)`, `agent_observe` restrictions
   (parent/siblings/children only), `rlm.create_session` for daemon-backed depth-0 sessions,
   `rlm.progress_note`, model inheritance and `thinking` override rules.
5. Continual harness block (prompt notes, memories, skills, subagent specs as compact digests,
   with routing guidance and `await refine.run()` triggers).
6. Available-skills inventory (name, type, python_import, description, location).
7. Prose style guidance (simplified technical English, progress updates, list usage).
8. Harness-digest: counts + recent refinements.

## Tool names exposed to the model

`bash`, `edit`, `ipython` (+ internal: `rename`, `stdout`).

## RLM kernel API (in the persistent Python REPL)

- `rlm.spawn(task, name)` -> child handle; `rlm.create_session('task', name=...)` (depth-0 only)
- `rlm.find_models(pattern)`; `rlm.list_subagents()`; `rlm.collect(targets, timeout_ms=0)`
- `rlm.delete_subagent(handle)`; `rlm.progress_note(...)`
- `rlm.harness`: create/update/delete for memory, skill, subagent, prompt_note;
  `record_refinement()`, `overview()`; `global_=` flag for cross-session entries
- `rlm.get_harness_state()`
- `agent_message.send(message, receiver_role=..., receiver_name=...)`
- `agent_observe.list_agents()` + bounded observation
- `goal` module: active goal read/complete
- `compact` module: context compaction
- `refine.run()`: turns patterns into harness entries (runs at turn end, returns immediately)
- `attach_image(path)`: loads an image into model context
- `bash(cmd)` background handles; `edit(path, old_str, new_str)` targeted edits

## Skill contract

Each skill has a SKILL.md with API docs; Python skills are pre-imported into the kernel
namespace; CLI fallback exists per skill. Skills listed in the system prompt with name, type,
python_import, description, location. Relative paths in SKILL.md resolve against the skill dir.

## Conversation log layout

`~/.prime/agent/sessions/<session-id>.jsonl` — append-only; one JSON object per line.
Session artifacts under `~/.prime/agent/session-artifacts/<session-id>/`.
