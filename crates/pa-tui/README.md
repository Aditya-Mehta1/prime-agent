# pa-tui

The terminal UI.

## Scope
Rendering (markdown, themes, layout, tool panels), editor (cursor/kill-ring/undo/history/word ops), keybindings (configurable, TS defaults), autocomplete, fullscreen/scrollback, input handling. Interactive sessions attached through the daemon: the JSONL client socket (hello handshake, command envelopes, session-event loop), slim-attach snapshot reconstruction, prompt submission with streamed assistant output, live session list and switch.

## Non-goals
No session logic, no providers, no loop policy. The interactive UI renders daemon events and sends user intents (prompts, abort, switch) as daemon commands; the session loop itself lives in the pa-daemon worker. It never computes agent behavior and never spawns the supervisor (launch semantics live in pa-cli).

## Public API
`app::run_app` (replay), `interactive::{run_interactive, InteractiveOptions, ModelSelection, SessionSelection, UiMode, HeadlessPlan, InteractiveOutcome}` (daemon-attached), `daemon_client::{DaemonClient, DaemonClientEvent}` (wire client for the UI and the composition root), `session::SessionStream`, theming, `interactive::{OnboardingTask, OnboardingSink}` (first-run flow surface; the composition root implements the sink against the settings manager). Component internals `pub(crate)`.

## Depends on
pa-types only (the daemon wire protocol types live there). The headless `UiMode` is the verifier seam: it drives the identical attach/submit/stream/render path without a TTY and captures rendered frames.
