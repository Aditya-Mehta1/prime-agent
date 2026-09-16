# pa-tui

The terminal UI.

## Scope
Rendering (markdown, themes, layout, tool panels), editor (cursor/kill-ring/undo/history/word ops), keybindings (configurable, TS defaults), autocomplete, fullscreen/scrollback, input handling.

## Non-goals
No session logic, no providers, no loop policy. The TUI renders a `SessionStream` (pa-types entries) and sends user intents upward; it never computes agent behavior.

## Public API
`TuiApp::run(stream, event source)`, `SessionStream` consumption, theming API. Component internals `pub(crate)`.

## Depends on
pa-types (one-way; daemon attach happens via the binary wiring in pa-cli).
