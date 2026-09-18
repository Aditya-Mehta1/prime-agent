//! The `/model` inline picker: the available-model catalog rendered through
//! the existing inline-picker component (TS `ModelSelectorComponent` reduced
//! to this seam — search, select, apply; Esc cancels). Enter applies the
//! selection through the caller; the picker itself owns only list state.

use crate::config_selector::{ConfigSelector, SelectorAction, SelectorKind, SelectorRow};
use crate::keybindings::KeybindingsManager;
use crate::theme::Theme;
use crate::Line;
use pa_types::ai::Model;

/// The session's current model, matched against the catalog so the picker
/// marks it checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentModel {
    pub provider: String,
    pub model_id: String,
}

/// One key press while the picker is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelPickerAction {
    /// Enter or Space on a model: the caller applies it.
    Apply { provider: String, model_id: String },
    /// Esc or Ctrl+C: close without applying.
    Cancel,
    /// Navigation or filter editing only.
    None,
}

/// The outcome of dispatching `/model [search]`.
#[derive(Debug)]
pub(crate) enum ModelCommandOutcome {
    /// Open the picker over the catalog.
    Open(ModelPicker),
    /// No models are available: the note to surface.
    NoModels(String),
}

/// The TS `formatNoModelsAvailableMessage` note for an empty catalog.
pub(crate) fn no_models_message() -> String {
    "No models available. Use /login to log into a provider via OAuth or API key, then retry /model".to_string()
}

/// Dispatch `/model [search]`: open the picker over `catalog` (an empty
/// catalog surfaces the TS no-models note instead), with `current` checked
/// and `search` as the prefilled filter.
pub(crate) fn model_command(
    catalog: &[Model],
    current: Option<&CurrentModel>,
    search: &str,
) -> ModelCommandOutcome {
    if catalog.is_empty() {
        return ModelCommandOutcome::NoModels(no_models_message());
    }
    let mut picker = ModelPicker::new(catalog, current);
    let search = search.trim();
    if !search.is_empty() {
        picker.set_query(search);
    }
    ModelCommandOutcome::Open(picker)
}

/// One picker over the available-model catalog. Rows carry the model's
/// index in the catalog as the identity key; the selector owns filtering,
/// navigation, and rendering.
#[derive(Debug)]
pub struct ModelPicker {
    selector: ConfigSelector,
    models: Vec<Model>,
}

impl ModelPicker {
    /// Build the picker: one item row per model (label = model id, filter
    /// fields = model id and provider), the current model checked.
    pub fn new(catalog: &[Model], current: Option<&CurrentModel>) -> Self {
        let rows = catalog
            .iter()
            .enumerate()
            .map(|(index, model)| SelectorRow::Item {
                key: index.to_string(),
                label: model.id.clone(),
                checked: current.is_some_and(|current| {
                    current.model_id == model.id && current.provider == model.provider
                }),
                type_label: model.provider.clone(),
                path: String::new(),
            })
            .collect();
        ModelPicker {
            selector: ConfigSelector::with_kind(rows, SelectorKind::Model),
            models: catalog.to_vec(),
        }
    }

    /// Prefill the filter (`/model <search>`; TS opens the selector with
    /// the search term applied).
    pub fn set_query(&mut self, query: &str) {
        self.selector.set_query(query);
    }

    /// The active filter query.
    pub fn query(&self) -> &str {
        self.selector.query()
    }

    /// The checked state of one catalog entry's row (the current or last
    /// applied model is checked).
    pub fn checked(&self, index: usize) -> Option<bool> {
        self.selector.checked(&index.to_string())
    }

    /// One key id. Cancel keys close without applying; Enter/Space map to
    /// the selector's toggle, which this picker reads as "apply the model
    /// at the selection" (single-select: exactly the picked row stays
    /// checked).
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> ModelPickerAction {
        // The full-screen selector loop treats Ctrl+C as process exit; the
        // in-chat overlay only cancels, like the TS model selector.
        if key == "ctrl+c" {
            return ModelPickerAction::Cancel;
        }
        match self.selector.handle_key(key, kb) {
            Some(SelectorAction::Close) => ModelPickerAction::Cancel,
            Some(SelectorAction::Exit) => ModelPickerAction::Cancel,
            Some(SelectorAction::Toggle { key, .. }) => self.apply_row(&key),
            None => ModelPickerAction::None,
        }
    }

    /// Apply the model whose row key is `key`, marking it the only checked
    /// row.
    fn apply_row(&mut self, key: &str) -> ModelPickerAction {
        let Ok(index) = key.parse::<usize>() else {
            return ModelPickerAction::None;
        };
        let Some(model) = self.models.get(index) else {
            return ModelPickerAction::None;
        };
        for position in 0..self.models.len() {
            self.selector
                .set_checked(&position.to_string(), position == index);
        }
        ModelPickerAction::Apply {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
        }
    }

    /// The picker's rendered frame (the inline-picker's bordered list).
    pub fn render(&self, theme: &Theme, width: usize) -> Vec<Line> {
        self.selector.render(theme, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};

    fn theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    fn kb() -> KeybindingsManager {
        KeybindingsManager::new()
    }

    /// A minimal catalog entry with the fields the picker reads (id and
    /// provider).
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
            "maxTokens": 1000
        }))
        .expect("mock model deserializes")
    }

    fn catalog() -> Vec<Model> {
        vec![
            model("anthropic", "claude-opus-4"),
            model("anthropic", "claude-haiku-4"),
            model("openai", "gpt-test"),
        ]
    }

    fn current() -> CurrentModel {
        CurrentModel {
            provider: "anthropic".to_string(),
            model_id: "claude-haiku-4".to_string(),
        }
    }

    #[test]
    fn marks_the_current_model_checked() {
        let picker = ModelPicker::new(&catalog(), Some(&current()));
        assert_eq!(picker.checked(1), Some(true));
        assert_eq!(picker.checked(0), Some(false));
        assert_eq!(picker.checked(2), Some(false));
    }

    #[test]
    fn enter_applies_the_selection_single_select() {
        let mut picker = ModelPicker::new(&catalog(), Some(&current()));
        // Enter on the first row (the initial selection).
        assert_eq!(
            picker.handle_key("enter", &kb()),
            ModelPickerAction::Apply {
                provider: "anthropic".to_string(),
                model_id: "claude-opus-4".to_string()
            }
        );
        // Single-select: the applied model is the only checked row.
        assert_eq!(picker.checked(0), Some(true));
        assert_eq!(picker.checked(1), Some(false));
    }

    #[test]
    fn escape_and_ctrl_c_cancel_without_applying() {
        let mut picker = ModelPicker::new(&catalog(), None);
        assert_eq!(picker.handle_key("escape", &kb()), ModelPickerAction::Cancel);
        assert_eq!(picker.handle_key("ctrl+c", &kb()), ModelPickerAction::Cancel);
    }

    #[test]
    fn navigation_moves_the_selection_and_enter_applies_it() {
        let mut picker = ModelPicker::new(&catalog(), None);
        assert_eq!(picker.handle_key("down", &kb()), ModelPickerAction::None);
        assert_eq!(picker.handle_key("down", &kb()), ModelPickerAction::None);
        assert_eq!(
            picker.handle_key("enter", &kb()),
            ModelPickerAction::Apply {
                provider: "openai".to_string(),
                model_id: "gpt-test".to_string()
            }
        );
    }

    #[test]
    fn filter_then_enter_applies_the_filtered_selection() {
        let mut picker = ModelPicker::new(&catalog(), None);
        picker.set_query("haiku");
        assert_eq!(picker.query(), "haiku");
        // The filtered view contains only the matching item.
        assert_eq!(
            picker.handle_key("enter", &kb()),
            ModelPickerAction::Apply {
                provider: "anthropic".to_string(),
                model_id: "claude-haiku-4".to_string()
            }
        );
    }

    #[test]
    fn filter_matches_the_provider_too() {
        let mut picker = ModelPicker::new(&catalog(), None);
        picker.set_query("openai");
        assert_eq!(
            picker.handle_key("enter", &kb()),
            ModelPickerAction::Apply {
                provider: "openai".to_string(),
                model_id: "gpt-test".to_string()
            }
        );
    }

    #[test]
    fn render_carries_the_model_header() {
        let picker = ModelPicker::new(&catalog(), None);
        let frame = picker.render(&theme(), 60);
        let text: Vec<String> = frame
            .iter()
            .map(|line| line.iter().map(|span| span.content.as_str()).collect())
            .collect();
        assert!(text.iter().any(|row| row.contains("Select Model")));
        assert!(text.iter().any(|row| row.contains("claude-opus-4")));
    }

    #[test]
    fn dispatch_opens_the_picker_and_prefills_the_search() {
        let outcome = model_command(&catalog(), None, " haiku ");
        let ModelCommandOutcome::Open(picker) = outcome else {
            panic!("expected the picker to open, got {outcome:?}")
        };
        assert_eq!(picker.query(), "haiku");
    }

    #[test]
    fn dispatch_with_an_empty_catalog_reports_no_models() {
        match model_command(&[], None, "") {
            ModelCommandOutcome::NoModels(message) => assert_eq!(message, no_models_message()),
            outcome => panic!("expected no-models, got {outcome:?}"),
        }
    }

    #[test]
    fn dispatch_without_search_opens_unfiltered() {
        let ModelCommandOutcome::Open(picker) = model_command(&catalog(), Some(&current()), "") else {
            panic!("expected the picker to open")
        };
        assert_eq!(picker.query(), "");
        assert_eq!(picker.checked(1), Some(true));
    }
}
