//! Global keybinding registry with TS DEFAULT_* defaults.
//!
//! Port of `packages/tui/src/keybindings.ts` + `coding-agent/src/core/keybindings.ts`.
//! Every binding is configurable via `~/.prime/agent/keybindings.json`; the
//! defaults below are the TS product's DEFAULT_* tables verbatim.

use anyhow::Result;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub default_keys: &'static [&'static str],
    #[allow(dead_code)]
    pub description: &'static str,
    pub default_key_scope: Option<&'static str>,
}

macro_rules! def {
    ($keys:expr, $desc:expr) => {
        KeybindingDefinition {
            default_keys: $keys,
            description: $desc,
            default_key_scope: None,
        }
    };
    ($keys:expr, $desc:expr, scope $scope:expr) => {
        KeybindingDefinition {
            default_keys: $keys,
            description: $desc,
            default_key_scope: Some($scope),
        }
    };
}

/// TUI-level bindings (`TUI_KEYBINDINGS`).
pub const TUI_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    (
        "tui.editor.cursorUp",
        def!(&["up"], "Move cursor up", scope "editor"),
    ),
    (
        "tui.editor.cursorDown",
        def!(&["down"], "Move cursor down", scope "editor"),
    ),
    (
        "tui.editor.cursorLeft",
        def!(&["left", "ctrl+b"], "Move cursor left", scope "editor"),
    ),
    (
        "tui.editor.cursorRight",
        def!(&["right", "ctrl+f"], "Move cursor right", scope "editor"),
    ),
    (
        "tui.editor.cursorWordLeft",
        def!(&["alt+left", "ctrl+left", "alt+b"], "Move cursor word left", scope "editor"),
    ),
    (
        "tui.editor.cursorWordRight",
        def!(&["alt+right", "ctrl+right", "alt+f"], "Move cursor word right", scope "editor"),
    ),
    (
        "tui.editor.cursorLineStart",
        def!(&["home", "ctrl+a"], "Move to line start", scope "editor"),
    ),
    (
        "tui.editor.cursorLineEnd",
        def!(&["end", "ctrl+e"], "Move to line end", scope "editor"),
    ),
    (
        "tui.editor.jumpForward",
        def!(&["ctrl+]"], "Jump forward to character", scope "editor"),
    ),
    (
        "tui.editor.jumpBackward",
        def!(&["ctrl+alt+]"], "Jump backward to character", scope "editor"),
    ),
    (
        "tui.editor.pageUp",
        def!(&["pageUp"], "Page up", scope "editor"),
    ),
    (
        "tui.editor.pageDown",
        def!(&["pageDown"], "Page down", scope "editor"),
    ),
    (
        "tui.editor.deleteCharBackward",
        def!(&["backspace"], "Delete character backward", scope "editor"),
    ),
    (
        "tui.editor.deleteCharForward",
        def!(&["delete", "ctrl+d"], "Delete character forward", scope "editor"),
    ),
    (
        "tui.editor.deleteWordBackward",
        def!(&["ctrl+w", "alt+backspace"], "Delete word backward", scope "editor"),
    ),
    (
        "tui.editor.deleteWordForward",
        def!(&["alt+d", "alt+delete"], "Delete word forward", scope "editor"),
    ),
    (
        "tui.editor.deleteToLineStart",
        def!(&["ctrl+u"], "Delete to line start", scope "editor"),
    ),
    (
        "tui.editor.deleteToLineEnd",
        def!(&["ctrl+k"], "Delete to line end", scope "editor"),
    ),
    ("tui.editor.yank", def!(&["ctrl+y"], "Yank", scope "editor")),
    (
        "tui.editor.yankPop",
        def!(&["alt+y"], "Yank pop", scope "editor"),
    ),
    ("tui.editor.undo", def!(&["ctrl+-"], "Undo", scope "editor")),
    (
        "tui.input.newLine",
        def!(&["shift+enter"], "Insert newline", scope "editor"),
    ),
    (
        "tui.input.submit",
        def!(&["enter"], "Submit input", scope "editor"),
    ),
    (
        "tui.input.tab",
        def!(&["tab"], "Tab / autocomplete", scope "editor"),
    ),
    (
        "tui.input.copy",
        def!(&["ctrl+c"], "Copy selection", scope "editor"),
    ),
    (
        "tui.viewport.pageUp",
        def!(&["pageUp"], "Scroll transcript up a page (fullscreen)"),
    ),
    (
        "tui.viewport.pageDown",
        def!(&["pageDown"], "Scroll transcript down a page (fullscreen)"),
    ),
    (
        "tui.viewport.top",
        def!(&["shift+alt+up"], "Scroll transcript to top (fullscreen)"),
    ),
    (
        "tui.viewport.follow",
        def!(
            &["ctrl+shift+down"],
            "Scroll to bottom and follow output (fullscreen)"
        ),
    ),
    ("tui.select.up", def!(&["up"], "Move selection up")),
    ("tui.select.down", def!(&["down"], "Move selection down")),
    ("tui.select.pageUp", def!(&["pageUp"], "Selection page up")),
    (
        "tui.select.pageDown",
        def!(&["pageDown"], "Selection page down"),
    ),
    ("tui.select.confirm", def!(&["enter"], "Confirm selection")),
    (
        "tui.select.cancel",
        def!(&["escape", "ctrl+c"], "Cancel selection"),
    ),
];

/// App-level bindings (`KEYBINDINGS` additions in coding-agent).
pub const APP_KEYBINDINGS: &[(&str, KeybindingDefinition)] = &[
    ("app.interrupt", def!(&[], "Interrupt current operation")),
    (
        "app.clear",
        def!(&["ctrl+c"], "Interrupt current operation, then exit"),
    ),
    (
        "app.input.clear",
        def!(&["escape"], "Interrupt response or clear prompt"),
    ),
    ("app.shortcuts", def!(&["?"], "Show keyboard shortcuts")),
    ("app.exit", def!(&["ctrl+d"], "Exit when editor is empty")),
    ("app.suspend", def!(&["ctrl+z"], "Suspend to background")),
    ("app.model.select", def!(&["ctrl+l"], "Open model selector")),
    (
        "app.model.toggleScope",
        def!(&["alt+s"], "Toggle model selector scope"),
    ),
    (
        "app.model.cycleForward",
        def!(&["alt+m"], "Cycle to the next scoped model"),
    ),
    (
        "app.model.cycleBackward",
        def!(&["shift+alt+m"], "Cycle to the previous scoped model"),
    ),
    (
        "app.tools.expand",
        def!(&["ctrl+o"], "Cycle conversation detail", scope "editor"),
    ),
    ("app.subagents.focus", def!(&["alt+a"], "Open child agents")),
    (
        "app.heartbeats.open",
        def!(&["ctrl+r"], "Manage heartbeats"),
    ),
    (
        "app.heartbeats.openSelected",
        def!(&["right"], "Open selected heartbeat"),
    ),
    (
        "app.editor.external",
        def!(&["ctrl+g"], "Open external editor"),
    ),
    (
        "app.prompt.stash",
        def!(&["ctrl+s"], "Stash or restore draft prompt"),
    ),
    (
        "app.message.followUp",
        def!(&["alt+enter"], "Queue follow-up message"),
    ),
    (
        "app.message.navigateOlder",
        def!(&["alt+up"], "Select older pending message"),
    ),
    (
        "app.message.navigateNewer",
        def!(&["alt+down"], "Select newer pending message or draft"),
    ),
    (
        "app.message.moveEarlier",
        def!(&["ctrl+alt+up"], "Move selected pending message earlier"),
    ),
    (
        "app.message.moveLater",
        def!(&["ctrl+alt+down"], "Move selected pending message later"),
    ),
    (
        "app.clipboard.pasteImage",
        def!(&["ctrl+v"], "Paste image from clipboard"),
    ),
    (
        "app.clipboard.copyLoginUrl",
        def!(&["c", "alt+c"], "Copy login URL"),
    ),
    ("app.session.new", def!(&[], "Start a new session")),
    ("app.session.tree", def!(&[], "Open session tree")),
    ("app.session.fork", def!(&[], "Fork current session")),
    ("app.session.resume", def!(&[], "Resume a session")),
    (
        "app.agents.back",
        def!(&["left"], "Return to parent agent scope"),
    ),
    (
        "app.agents.open",
        def!(&["right"], "Drill into selected agent"),
    ),
    (
        "app.modal.back",
        def!(&["left"], "Go back / close the current dialog"),
    ),
    (
        "app.agents.reply",
        def!(&["space"], "Reply to selected agent"),
    ),
    (
        "app.agents.new",
        def!(&["ctrl+n"], "Start a new session from the agents view"),
    ),
    (
        "app.agents.delete",
        def!(&["ctrl+x"], "Stop or delete selected agent"),
    ),
    (
        "app.agents.program",
        def!(&["ctrl+o"], "Show the program that spawned subagents"),
    ),
    (
        "app.agents.rename",
        def!(&["ctrl+r"], "Rename selected agent session"),
    ),
    (
        "app.agents.expand",
        def!(
            &["alt+right"],
            "Expand or collapse selected agent subagents"
        ),
    ),
    (
        "app.tree.foldOrUp",
        def!(&["ctrl+left", "alt+left"], "Fold tree branch or move up"),
    ),
    (
        "app.tree.unfoldOrDown",
        def!(
            &["ctrl+right", "alt+right"],
            "Unfold tree branch or move down"
        ),
    ),
    ("app.tree.editLabel", def!(&["shift+l"], "Edit tree label")),
    (
        "app.tree.toggleLabelTimestamp",
        def!(&["shift+t"], "Toggle tree label timestamps"),
    ),
    ("app.models.save", def!(&["ctrl+s"], "Save model selection")),
    (
        "app.models.enableAll",
        def!(&["ctrl+a"], "Enable all models"),
    ),
    ("app.models.clearAll", def!(&["ctrl+x"], "Clear all models")),
    (
        "app.models.toggleProvider",
        def!(&["ctrl+p"], "Toggle all models for provider"),
    ),
    (
        "app.models.reorderUp",
        def!(&["alt+up"], "Move model up in order"),
    ),
    (
        "app.models.reorderDown",
        def!(&["alt+down"], "Move model down in order"),
    ),
    (
        "app.tree.filter.default",
        def!(&["ctrl+d"], "Tree filter: default view"),
    ),
    (
        "app.tree.filter.noTools",
        def!(&["ctrl+t"], "Tree filter: hide tool results"),
    ),
    (
        "app.tree.filter.userOnly",
        def!(&["ctrl+u"], "Tree filter: user messages only"),
    ),
    (
        "app.tree.filter.labeledOnly",
        def!(&["ctrl+l"], "Tree filter: labeled entries only"),
    ),
    (
        "app.tree.filter.all",
        def!(&["ctrl+a"], "Tree filter: show all entries"),
    ),
    (
        "app.tree.filter.cycleForward",
        def!(&["ctrl+o"], "Tree filter: cycle forward"),
    ),
    (
        "app.tree.filter.cycleBackward",
        def!(&["shift+ctrl+o"], "Tree filter: cycle backward"),
    ),
];

pub type KeybindingsConfig = BTreeMap<String, Vec<String>>;

/// Resolved binding table: definition defaults overlaid with user config.
pub struct KeybindingsManager {
    definitions: BTreeMap<&'static str, KeybindingDefinition>,
    resolved: BTreeMap<String, Vec<String>>,
    user_bindings: KeybindingsConfig,
}

fn all_definitions() -> BTreeMap<&'static str, KeybindingDefinition> {
    let mut map = BTreeMap::new();
    for (id, definition) in TUI_KEYBINDINGS.iter().chain(APP_KEYBINDINGS.iter()) {
        map.insert(*id, definition.clone());
    }
    map
}

impl KeybindingsManager {
    /// All definitions with the TS defaults.
    pub fn new() -> Self {
        Self::with_user_bindings(KeybindingsConfig::new())
    }

    pub fn with_user_bindings(user_bindings: KeybindingsConfig) -> Self {
        let definitions = all_definitions();
        let mut manager = Self {
            definitions,
            resolved: BTreeMap::new(),
            user_bindings,
        };
        manager.rebuild();
        manager
    }

    /// Mirrors KeybindingsManager.rebuild(): explicit user claims win; within
    /// the same default scope, a user-claimed key frees other defaults of it.
    fn rebuild(&mut self) {
        // Mirrors TS KeybindingsManager.rebuild(): track which key strings were
        // explicitly claimed by user bindings outside their definition defaults;
        // those keys are freed from other defaults in the same scope.
        let mut added_claims: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, keys) in &self.user_bindings {
            let Some(definition) = self.definitions.get(id.as_str()) else {
                continue;
            };
            for key in keys {
                if !definition.default_keys.contains(&key.as_str()) {
                    added_claims
                        .entry(key.clone())
                        .or_default()
                        .push(id.clone());
                }
            }
        }
        self.resolved.clear();
        for (id, definition) in &self.definitions {
            if let Some(keys) = self.user_bindings.get(*id) {
                self.resolved.insert(id.to_string(), keys.clone());
                continue;
            }
            let keys: Vec<String> = definition
                .default_keys
                .iter()
                .filter(|key| {
                    let Some(scope) = definition.default_key_scope else {
                        return true;
                    };
                    let key_str: &str = key;
                    !added_claims.get(key_str).is_some_and(|claimants| {
                        claimants.iter().any(|c| {
                            self.definitions
                                .get(c.as_str())
                                .and_then(|d| d.default_key_scope)
                                == Some(scope)
                        })
                    })
                })
                .map(|k| k.to_string())
                .collect();
            self.resolved.insert(id.to_string(), keys);
        }
    }

    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.resolved
            .get(keybinding)
            .is_some_and(|keys| keys.iter().any(|k| k == data))
    }

    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.resolved.get(keybinding).cloned().unwrap_or_default()
    }

    pub fn first_key(&self, keybinding: &str) -> Option<String> {
        self.get_keys(keybinding).into_iter().next()
    }

    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(keybinding)
    }

    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// Load user bindings from a keybindings.json file (missing file = defaults).
    pub fn load_from_file(path: &std::path::Path) -> Result<Self> {
        let user_bindings = load_config(path)?;
        Ok(Self::with_user_bindings(user_bindings))
    }
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        Self::new()
    }
}

fn load_config(path: &std::path::Path) -> Result<KeybindingsConfig> {
    let mut config = KeybindingsConfig::new();
    if !path.exists() {
        return Ok(config);
    }
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let Some(obj) = raw.as_object() else {
        return Ok(config);
    };
    for (key, value) in obj {
        if let Some(s) = value.as_str() {
            config.insert(key.clone(), vec![s.to_string()]);
        } else if let Some(arr) = value.as_array() {
            let keys: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !keys.is_empty() {
                config.insert(key.clone(), keys);
            }
        }
    }
    Ok(config)
}

/// Format a key id for display in hints ("ctrl+o" -> "Ctrl+O", arrows to glyphs).
pub fn format_key_text(key: &str) -> String {
    key.split('/')
        .map(|binding| {
            binding
                .split('+')
                .map(|part| match part {
                    "escape" => "Esc".to_string(),
                    "up" => "\u{2191}".to_string(),
                    "down" => "\u{2193}".to_string(),
                    "left" => "\u{2190}".to_string(),
                    "right" => "\u{2192}".to_string(),
                    "pageUp" => "PageUp".to_string(),
                    "pageDown" => "PageDown".to_string(),
                    other => {
                        let mut c = other.chars();
                        match c.next() {
                            Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                            None => String::new(),
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_ts() {
        let kb = KeybindingsManager::new();
        assert!(kb.matches("ctrl+o", "app.tools.expand"));
        assert!(kb.matches("escape", "app.input.clear"));
        assert!(kb.matches("ctrl+shift+down", "tui.viewport.follow"));
        assert!(kb.matches("shift+alt+up", "tui.viewport.top"));
        assert!(kb.matches("ctrl+w", "tui.editor.deleteWordBackward"));
        assert!(kb.matches("alt+backspace", "tui.editor.deleteWordBackward"));
        assert!(kb.matches("ctrl+-", "tui.editor.undo"));
        assert!(kb.matches("shift+enter", "tui.input.newLine"));
        assert!(!kb.matches("ctrl+o", "app.clear"));
    }

    #[test]
    fn user_rebind_supersedes_scope_default() {
        let mut cfg = KeybindingsConfig::new();
        cfg.insert("app.tools.expand".into(), vec!["ctrl+e".into()]);
        let kb = KeybindingsManager::with_user_bindings(cfg);
        assert!(kb.matches("ctrl+e", "app.tools.expand"));
        // ctrl+e is also editor cursorLineEnd default in the same scope: claimed => removed there.
        assert!(!kb.matches("ctrl+e", "tui.editor.cursorLineEnd"));
        assert!(kb.matches("end", "tui.editor.cursorLineEnd"));
    }

    #[test]
    fn formats_key_text() {
        assert_eq!(format_key_text("ctrl+o"), "Ctrl+O");
        assert_eq!(format_key_text("shift+alt+up"), "Shift+Alt+\u{2191}");
        assert_eq!(format_key_text("escape"), "Esc");
    }
}
