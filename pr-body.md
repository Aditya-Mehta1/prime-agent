## pa-models/pa-ai: live-catalog thinking/allowed-parameter metadata drives reasoning controls (port of #2519)

### The regression

Prime Inference rejects `enable_thinking` on GLM routes with a 400 (`Validation: Unsupported parameter(s): enable_thinking`), killing every reasoning request on those routes. The deeper root cause: the Rust port does NOT get thinking/allowed-parameter metadata from the live pinference catalog — `crates/pa-models/src/prime_inference.rs:320` resolved `thinking_level_map` from the **compiled pinning templates** (`template.and_then(|t| t.thinking_level_map.clone())`), so the live refresh kept stale compiled reasoning controls even when the gateway's declarations had moved on.

TS fixed exactly this in **org PR #2519** (*use the prime inference catalog thinking metadata for reasoning controls*): the live catalog's `supported_parameters`/`reasoning` metadata now drives the reasoning controls — which routes get `enable_thinking`, which send only their declared `reasoning_effort` values, and which switch reasoning through the declared `reasoning` object. This PR ports that approach to the rust branch.

### The fix

**1. Live catalog reasoning controls (`crates/pa-models/src/prime_inference.rs`)** — port of `packages/ai/src/prime-inference-model-catalog.ts` + the `buildPrimeInferenceModels` change in `packages/coding-agent/src/core/prime-inference-model-catalog.ts`:

- the parser reads `supported_parameters`, `reasoning.supported_efforts` and `reasoning.mandatory` from the live `/models` payload (non-strings drop, duplicates collapse, only a `true` mandatory is kept — `parseStringArray` parity);
- `prime_inference_reasoning_controls` (the `getPrimeInferenceReasoningControls` port) derives the request shape: **effort routes** send only their declared `reasoning_effort` values (`off` → `none` when allowed, hidden when mandatory), **toggle-only routes** switch reasoning through the declared `reasoning` object (openrouter thinking format), **`enable_thinking` routes** keep the zai format;
- `build_prime_inference_models` applies the controls to compat (`supportsReasoningEffort` always; `thinkingFormat` set or removed) and rebuilds the thinking-level map from the declarations — routes without declarations keep their bundled template, never guess.

**2. Regenerated compiled pinning (`crates/pa-ai/src/models.generated.json`)** — the #2519 regeneration of `models.generated.ts`, ported at field level onto our pinning (56 entries; the exact same field deltas the TS PR applies):

- effort routes gain `off: "none"` (mandatory routes keep `off: null`); non-reasoning routes drop the default `reasoning_effort` advertising (`supportsReasoningEffort: false`);
- `z-ai/glm-4.5`…`glm-5.1` become reasoning-object toggles (`thinkingFormat: "openrouter"`); `z-ai/glm-5.2`, `glm-5.3`, `glm-5.3-flash` become effort routes (`supportsReasoningEffort: true`);
- deepseek-v4 routes drop the `deepseek` thinking format (their live routes declare `reasoning_effort`) and get catalog-aligned maps;
- `z-ai/glm-5.3-flash` `maxTokens` picks up the live spec (943718), as in the TS regeneration.

Supersedes the interim #2459 port already on this branch (commit `cdaa87393` dropped the zai format so nothing was sent; the live-catalog approach now sends exactly the declared shapes — keeping the format drop, adding the declared efforts/toggles).

**3. Regression tests (ported from #2519's)** — params payload shapes (glm-5.3 → `reasoning_effort` with medium clamping to the declared high; glm-4.7 → `reasoning: {enabled}` toggle, `off` → `enabled: false`; direct z.ai keeps `enable_thinking`), declaration parsing/sanitizing, the four control shapes, the stale-template build cases (effort/toggle/kept/dropped), conservative default compat for template-less live models, and catalog pins of the regenerated GLM entries. The #2459 catalog guard (no prime-inference route carries the zai format) stays.

### Parity evidence

TS reference: PR #2519 (head `afb8248a`), files `packages/ai/src/prime-inference-model-catalog.ts`, `packages/coding-agent/src/core/prime-inference-model-catalog.ts`, `packages/ai/scripts/generate-models.ts`, `packages/ai/src/models.generated.ts`, tests in `packages/ai/test/*` + `packages/coding-agent/test/*`. Rust correspondence:

| TS | Rust |
|---|---|
| `getPrimeInferenceReasoningControls` + `parseStringArray` | `pa-models::prime_inference::{prime_inference_reasoning_controls, parse_string_array}` |
| `PrimeInferenceCatalogEntry.{supportedParameters,reasoningEfforts,reasoningMandatory}` | `PrimeInferenceEntry.{supported_parameters,reasoning_efforts,reasoning_mandatory}` |
| coding-agent `buildPrimeInferenceModels` controls override | `build_prime_inference_models` compat/map override |
| regenerated `models.generated.ts` (59 entries) | field-level deltas on 56 entries of `models.generated.json` |
| `applyThinkingLevelMetadata` prime-inference skip | baked into the pre-generated pinning (the Rust catalog is pre-generated, no runtime heuristics) |

Sanctioned divergences (pre-existing pinning drift, not #2519 scope): 3 entries the TS PR touches do not exist in our pinning (`Qwen/Qwen3-235B-A22B-Instruct-2507`, `deepseek/deepseek-v4.1-flash`, `zai-org/GLM-4.7`); `glm-5.3-flash` `maxTokens` takes the PR's live-spec value (our pinning carried an older 131072, the TS base 102400).

### Gates

VM gate evidence to follow (slot-queued): `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --D warnings`, `cargo test --workspace` in a rust:1-bookworm Prime VM sandbox.
