# pa-tui

The terminal UI.

## Scope
Rendering (markdown, themes, layout, tool panels, custom-message decorated rows), editor (cursor/kill-ring/undo/history/word ops), keybindings (configurable, TS defaults), autocomplete, fullscreen/scrollback, input handling. Interactive sessions attached through the daemon: the JSONL client socket (hello handshake, command envelopes, session-event loop), slim-attach snapshot reconstruction, prompt submission with streamed assistant output, live session list and switch, and the agents view (the unified live-roster +
saved-catalog session list: Running/Idle/Inactive sections, inline search,
open-to-attach/resume, `n`-via-ctrl+n new session, live `roster_subscribe`
pushes).

## Non-goals
No session logic, no providers, no loop policy. The interactive UI renders daemon events and sends user intents (prompts, abort, switch) as daemon commands; the session loop itself lives in the pa-daemon worker. It never computes agent behavior and never spawns the supervisor (launch semantics live in pa-cli).

## Public API
`custom_message::{custom_message_entries, AGENT_MESSAGE_CUSTOM_TYPE, ...}` (the custom-type render dispatch shared by the live and replay paths; component row renderers are `pub(crate)`), `app::run_app` (replay), `interactive::{run_interactive, InteractiveOptions, ModelSelection, SessionSelection, UiMode, HeadlessPlan, InteractiveOutcome}` (daemon-attached), `daemon_client::{DaemonClient, DaemonClientEvent}` (wire client for the UI and the composition root), `session::SessionStream`, `agents_view::{run_agents_view, AgentsViewOptions, AgentsViewUiMode, AgentsViewOutcome}` (the view and its headless verifier seam), `interactive::{OnboardingTask, OnboardingSink}` (first-run flow surface; the composition root implements the sink against the settings manager), `client_auth::{ClientAuthCommands, ClientAuthCommandsHandle, run_mcp_auth_command}` (the `/mcp login`/`/mcp logout` client surface: the TUI owns the command UX — TS usage wording, the terminal-suspend seam that hands the TTY to the flow — while the composition root runs the login itself). Component internals `pub(crate)`. `altscreen::{enter, leave}` is the process-wide alternate-screen ownership shared by the terminal surfaces: the first surface enters the screen, a real exit leaves it, and in-process view switches adopt the same buffer (no primary-screen flash between the agents view and a chat).

## Depends on
pa-types only (the daemon wire protocol types live there). The headless `UiMode` is the verifier seam: it drives the identical attach/submit/stream/render path without a TTY and captures rendered frames.
