# pa-ai

Provider APIs and model registry.

## Scope
Provider trait + per-provider streaming clients (anthropic, openai-completions/responses, google, bedrock, mistral, azure, prime-inference), model registry/resolution, usage accounting, stream-failure retry, overflow handling, JSON repair parsing, faux provider for tests.

## Non-goals
No agent loop, no tool execution, no session state, no UI. Receives/returns `pa-types` messages.

## Public API
`Provider` trait, `ProviderRegistry`, model lookup/resolution, faux provider. Per-provider internals are `pub(crate)`.

## Depends on
pa-types (one-way).
