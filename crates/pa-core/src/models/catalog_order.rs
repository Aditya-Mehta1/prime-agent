//! Picker catalog ordering: the order the `/model` selector lists models
//! in. The composition root (pa-cli) has already filtered the catalog to
//! available models (auth-configured), so this owns the remaining order:
//! the current model first, then the recent-use rank, the provider name,
//! the featured flag, and finally a natural id compare. Signed-in Prime
//! Inference models pin above the rest, like the TS selector's provider
//! pinning.

use std::cmp::Ordering;

use pa_types::ai::Model;

use crate::auth::types::PRIME_INFERENCE_PROVIDER_ID;

/// The `(provider, model id)` identity of one catalog entry (the TS
/// `modelsAreEqual` key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerModel {
    pub provider: String,
    pub id: String,
}

impl PickerModel {
    /// The identity of one catalog entry.
    pub fn of(model: &Model) -> PickerModel {
        PickerModel {
            provider: model.provider.clone(),
            id: model.id.clone(),
        }
    }

    /// The settings recent-models key (`provider/id`).
    fn recent_key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// Order the picker catalog. `current` is the session's model; `recent` is
/// the settings recent-model list (`provider/id` keys, most recent first).
pub fn order_for_picker(
    models: Vec<Model>,
    current: Option<&PickerModel>,
    recent: &[String],
) -> Vec<Model> {
    let mut ordered = models;
    ordered.sort_by(|a, b| compare(a, b, current, recent));
    ordered
}

fn compare(a: &Model, b: &Model, current: Option<&PickerModel>, recent: &[String]) -> Ordering {
    // Signed-in Prime Inference models pin above the rest (the TS
    // `isPinnedProvider` criterion).
    let pinned = |model: &Model| model.provider == PRIME_INFERENCE_PROVIDER_ID;
    if pinned(a) != pinned(b) {
        return if pinned(a) {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    // The current model leads the list.
    let is_current =
        |model: &Model| current.is_some_and(|current| current == &PickerModel::of(model));
    let (a_current, b_current) = (is_current(a), is_current(b));
    if a_current != b_current {
        return if a_current {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    // Then the recent-use rank (ascending; the sentinel sorts last).
    let recent_rank = |model: &Model| {
        recent
            .iter()
            .position(|entry| *entry == PickerModel::of(model).recent_key())
            .unwrap_or(usize::MAX)
    };
    let (a_rank, b_rank) = (recent_rank(a), recent_rank(b));
    if a_rank != b_rank {
        return a_rank.cmp(&b_rank);
    }
    // Then the provider name.
    if a.provider != b.provider {
        return a.provider.cmp(&b.provider);
    }
    // Featured models of the same provider lead.
    let featured = |model: &Model| model.featured == Some(true);
    if featured(a) != featured(b) {
        return if featured(a) {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    // Finally the model id, numeric-aware (`localeCompare` with
    // `numeric: true`): `m-2` sorts before `m-10`.
    natural_cmp(&a.id, &b.id)
}

/// Numeric-aware string compare: digit runs compare by value, everything
/// else by characters (`localeCompare` with `numeric: true`, so `m-2`
/// sorts before `m-10`).
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a_rest = a;
    let mut b_rest = b;
    loop {
        match (split_run(a_rest), split_run(b_rest)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((a_run, a_digits)), Some((b_run, b_digits))) => {
                let run_order = if a_digits && b_digits {
                    // Equal values keep the shorter digit run first so the
                    // order stays total.
                    match numeric_value_cmp(&a_run, &b_run) {
                        Ordering::Equal => a_run.len().cmp(&b_run.len()),
                        other => other,
                    }
                } else if a_digits == b_digits {
                    a_run.cmp(&b_run)
                } else {
                    // A digit run against a non-digit run: compare by the
                    // first character so ordering stays stable.
                    a_run.chars().next().cmp(&b_run.chars().next())
                };
                if run_order != Ordering::Equal {
                    return run_order;
                }
                a_rest = &a_rest[a_run.len()..];
                b_rest = &b_rest[b_run.len()..];
            }
        }
    }
}

/// Split one run (digits or non-digits) off the head of `text`.
fn split_run(text: &str) -> Option<(String, bool)> {
    let first = text.chars().next()?;
    let digits = first.is_ascii_digit();
    let end = text
        .char_indices()
        .find(|(_, character)| character.is_ascii_digit() != digits)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    Some((text[..end].to_string(), digits))
}

/// Numeric compare of two digit runs (leading zeros ignored).
fn numeric_value_cmp(a: &str, b: &str) -> Ordering {
    let a_digits = a.trim_start_matches('0');
    let b_digits = b.trim_start_matches('0');
    if a_digits.len() != b_digits.len() {
        return a_digits.len().cmp(&b_digits.len());
    }
    a_digits.cmp(b_digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, id: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": "https://example.invalid/v1",
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000,
            "maxTokens": 1000,
            "featured": serde_json::Value::Null,
        }))
        .expect("mock model deserializes")
    }

    /// A catalog entry flagged featured.
    fn featured_model(provider: &str, id: &str) -> Model {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": id,
            "api": "openai-completions",
            "provider": provider,
            "baseUrl": "https://example.invalid/v1",
            "reasoning": false,
            "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000,
            "maxTokens": 1000,
            "featured": true,
        }))
        .expect("mock model deserializes")
    }

    fn ids(models: &[Model]) -> Vec<String> {
        models.iter().map(|model| model.id.clone()).collect()
    }

    fn current(provider: &str, id: &str) -> PickerModel {
        PickerModel {
            provider: provider.to_string(),
            id: id.to_string(),
        }
    }

    #[test]
    fn current_model_leads() {
        let models = vec![
            model("prime-inference", "aaa"),
            model("prime-inference", "mock-1"),
            model("prime-inference", "zzz"),
        ];
        let ordered = order_for_picker(models, Some(&current("prime-inference", "mock-1")), &[]);
        assert_eq!(
            ids(&ordered),
            ["mock-1", "aaa", "zzz"].map(String::from).to_vec()
        );
    }

    #[test]
    fn prime_inference_pins_above_other_providers() {
        let models = vec![
            model("anthropic", "claude"),
            model("prime-inference", "glm"),
        ];
        let ordered = order_for_picker(models, None, &[]);
        assert_eq!(ids(&ordered), ["glm", "claude"].map(String::from).to_vec());
    }

    #[test]
    fn recent_rank_orders_before_provider_name() {
        let models = vec![
            model("prime-inference", "a-first"),
            model("prime-inference", "b-recent"),
        ];
        let ordered = order_for_picker(models, None, &["prime-inference/b-recent".to_string()]);
        assert_eq!(
            ids(&ordered),
            ["b-recent", "a-first"].map(String::from).to_vec()
        );
    }

    #[test]
    fn featured_leads_within_a_provider() {
        let models = vec![
            model("prime-inference", "plain"),
            featured_model("prime-inference", "star"),
        ];
        let ordered = order_for_picker(models, None, &[]);
        assert_eq!(ids(&ordered), ["star", "plain"].map(String::from).to_vec());
    }

    #[test]
    fn ids_compare_numerically() {
        let models = vec![
            model("prime-inference", "m-10"),
            model("prime-inference", "m-2"),
            model("prime-inference", "m-1"),
        ];
        let ordered = order_for_picker(models, None, &[]);
        assert_eq!(
            ids(&ordered),
            ["m-1", "m-2", "m-10"].map(String::from).to_vec()
        );
    }

    #[test]
    fn natural_compare_pairs() {
        assert_eq!(natural_cmp("a2b", "a10b"), Ordering::Less);
        assert_eq!(natural_cmp("a10b", "a2b"), Ordering::Greater);
        assert_eq!(natural_cmp("same", "same"), Ordering::Equal);
        assert_eq!(natural_cmp("abc", "abd"), Ordering::Less);
        assert_eq!(natural_cmp("abc", "abcde"), Ordering::Less);
        assert_eq!(natural_cmp("a", "a1"), Ordering::Less);
    }
}
