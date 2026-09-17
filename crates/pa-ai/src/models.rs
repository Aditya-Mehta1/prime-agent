//! Model helpers: cost calculation, thinking-level support, equality.
//! Ported from `packages/ai/src/models.ts` (model registry plumbing excluded —
//! the generated catalog is ported separately).

use std::collections::HashMap;

use pa_types::JsNumber;

use crate::types::{Model, ModelThinkingLevel, Usage, UsageCost};

/// EXTENDED_THINKING_LEVELS in the TS reference.
pub const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

pub const SUPPORTED_THINKING_LEVELS: [ModelThinkingLevel; 7] = EXTENDED_THINKING_LEVELS;

/// Provider-side helpers over [`ModelThinkingLevel`] (the type is owned by
/// `pa-types`, so they live on an extension trait).
pub trait ModelThinkingLevelExt {
    /// Ordinal position within [`EXTENDED_THINKING_LEVELS`].
    fn index(self) -> usize;
    /// Wire value used in `thinkingLevelMap` keys.
    fn wire_name(self) -> &'static str;
}

impl ModelThinkingLevelExt for ModelThinkingLevel {
    fn index(self) -> usize {
        EXTENDED_THINKING_LEVELS
            .iter()
            .position(|level| *level == self)
            .expect("thinking level is always in EXTENDED_THINKING_LEVELS")
    }

    fn wire_name(self) -> &'static str {
        ModelThinkingLevel::wire_name(self)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CostOverrides {
    pub cache_write: Option<f64>,
}

/// Compute and write the cost breakdown for a usage, in place on `usage.cost`.
pub fn calculate_cost(model: &Model, usage: &mut Usage, overrides: Option<&CostOverrides>) {
    usage.cost = calculate_cost_values(model, usage, overrides);
}

pub fn calculate_cost_values(
    model: &Model,
    usage: &Usage,
    overrides: Option<&CostOverrides>,
) -> UsageCost {
    let cache_write_cost = overrides
        .and_then(|overrides| overrides.cache_write)
        .unwrap_or_else(|| model.cost.cache_write.as_f64());
    let input = (model.cost.input.as_f64() / 1_000_000.0) * usage.input as f64;
    let output = (model.cost.output.as_f64() / 1_000_000.0) * usage.output as f64;
    let cache_read = (model.cost.cache_read.as_f64() / 1_000_000.0) * usage.cache_read as f64;
    let cache_write = (cache_write_cost / 1_000_000.0) * usage.cache_write as f64;
    UsageCost {
        input: JsNumber::from(input),
        output: JsNumber::from(output),
        cache_read: JsNumber::from(cache_read),
        cache_write: JsNumber::from(cache_write),
        total: JsNumber::from(input + output + cache_read + cache_write),
    }
}

/// Thinking levels the model supports: "off" always when non-reasoning;
/// otherwise every level that is not explicitly mapped to null. `xhigh`/`max`
/// additionally require an explicit mapping.
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(level));
            match mapped {
                None => !matches!(level, ModelThinkingLevel::Xhigh | ModelThinkingLevel::Max),
                Some(None) => false,
                Some(Some(_)) => true,
            }
        })
        .collect()
}

/// Clamp a requested thinking level to what the model supports, preferring the
/// nearest higher level then the nearest lower one.
pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let requested_index = level.index();
    for candidate in EXTENDED_THINKING_LEVELS.iter().skip(requested_index) {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}

/// Parse a thinking level from its wire name ("off", "minimal", ...).
pub fn thinking_level_from_str(name: &str) -> Option<ModelThinkingLevel> {
    EXTENDED_THINKING_LEVELS
        .iter()
        .find(|level| level.wire_name() == name)
        .copied()
}

/// Build a thinking level map from pairs (helper for tests and catalogs).
pub fn thinking_level_map(
    pairs: &[(ModelThinkingLevel, Option<&str>)],
) -> HashMap<ModelThinkingLevel, Option<String>> {
    pairs
        .iter()
        .map(|(key, value)| (*key, value.map(|v| v.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput};
    use pa_types::JsNumber;

    fn model(reasoning: bool, map: Option<HashMap<ModelThinkingLevel, Option<String>>>) -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning,
            thinking_level_map: map,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: JsNumber::from(0.0),
                output: JsNumber::from(0.0),
                cache_read: JsNumber::from(0.0),
                cache_write: JsNumber::from(0.0),
            },
            context_window: 128_000,
            max_tokens: 8192,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn computes_cost() {
        let model = Model {
            cost: ModelCost {
                input: JsNumber::from(10.0),
                output: JsNumber::from(50.0),
                cache_read: JsNumber::from(1.0),
                cache_write: JsNumber::from(12.5),
            },
            ..model(false, None)
        };
        let mut usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 10_000,
            cache_write: 20_000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage, None);
        assert!((usage.cost.input.as_f64() - 10.0).abs() < 1e-9);
        assert!((usage.cost.output.as_f64() - 5.0).abs() < 1e-9);
        assert!((usage.cost.cache_read.as_f64() - 0.01).abs() < 1e-9);
        assert!((usage.cost.cache_write.as_f64() - 0.25).abs() < 1e-9);
        assert!((usage.cost.total.as_f64() - (10.0 + 5.0 + 0.01 + 0.25)).abs() < 1e-9);
    }

    #[test]
    fn clamps_thinking_levels() {
        // off: null means disabled; xhigh/max require explicit mapping.
        let map = thinking_level_map(&[
            (ModelThinkingLevel::Off, Some("none")),
            (ModelThinkingLevel::Low, Some("low")),
            (ModelThinkingLevel::Medium, None),
            (ModelThinkingLevel::High, Some("high")),
        ]);
        let m = model(true, Some(map));
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::Off),
            ModelThinkingLevel::Off
        );
        // TS ground truth (verified against packages/ai/src/models.ts): minimal is
        // always supported on reasoning models; medium maps to null (unsupported)
        // so it clamps up to high; xhigh requires an explicit mapping and clamps down
        // to high via the nearest-higher-then-lower rule.
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::Minimal),
            ModelThinkingLevel::Minimal
        );
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::Medium),
            ModelThinkingLevel::High
        );
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::Xhigh),
            ModelThinkingLevel::High
        );
    }

    #[test]
    fn non_reasoning_models_only_support_off() {
        let m = model(false, None);
        assert_eq!(
            get_supported_thinking_levels(&m),
            vec![ModelThinkingLevel::Off]
        );
        assert_eq!(
            clamp_thinking_level(&m, ModelThinkingLevel::High),
            ModelThinkingLevel::Off
        );
    }

    #[test]
    fn wire_names_match_ts_keys() {
        assert_eq!(ModelThinkingLevel::Off.wire_name(), "off");
        assert_eq!(ModelThinkingLevel::Max.wire_name(), "max");
        assert_eq!(
            thinking_level_from_str("xhigh"),
            Some(ModelThinkingLevel::Xhigh)
        );
        assert_eq!(thinking_level_from_str("nope"), None);
    }
}
