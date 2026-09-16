//! `SettingsManager`: loads global + project settings, merges them, tracks
//! modified fields, and writes back only what this session changed.

use std::sync::Arc;

use anyhow::Result;

use super::load::from_value_lenient;
use super::merge::{deep_merge, migrate};
use super::storage::{SettingsScope, SettingsStorage};
use super::types::{
    QueueModeSetting, Settings, ThinkingLevelSetting, TransportSetting, UpdateChannel,
};

pub const RECENT_MODELS_LIMIT: usize = 20;
pub const DEFAULT_IDLE_EVICTION_MINUTES: u64 = 90;

#[derive(Debug, Clone)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub message: String,
}

pub struct SettingsManager {
    storage: Arc<dyn SettingsStorage>,
    global: Settings,
    project: Settings,
    merged: Settings,
    runtime_overrides: Settings,
    errors: Vec<SettingsError>,
    /// Load failures per scope; a scope whose file failed to parse is never
    /// written back (the TS `save` guard against clobbering bad settings).
    global_load_error: Option<String>,
    project_load_error: Option<String>,
}

impl SettingsManager {
    /// Load global + project settings from a storage backend.
    pub fn from_storage(storage: Arc<dyn SettingsStorage>) -> Self {
        let mut errors = Vec::new();
        let global = load_scope(storage.as_ref(), SettingsScope::Global, &mut errors);
        let project = load_scope(storage.as_ref(), SettingsScope::Project, &mut errors);
        let (global, global_load_error) = global;
        let (project, project_load_error) = project;
        let merged = deep_merge(&global, &project);
        Self {
            storage,
            global,
            project,
            merged,
            runtime_overrides: Settings::default(),
            errors,
            global_load_error,
            project_load_error,
        }
    }

    /// File-backed manager (agentDir + cwd/.prime/agent).
    pub fn create(
        cwd: impl AsRef<std::path::Path>,
        agent_dir: impl AsRef<std::path::Path>,
    ) -> Self {
        let cwd = std::path::PathBuf::from(cwd.as_ref());
        let agent_dir = std::path::PathBuf::from(agent_dir.as_ref());
        let storage: Arc<dyn SettingsStorage> =
            Arc::new(super::storage::FileSettingsStorage::new(cwd, agent_dir));
        Self::from_storage(storage)
    }

    /// In-memory manager (tests, embedded hosts).
    pub fn in_memory(initial: Settings) -> Self {
        let storage: Arc<dyn SettingsStorage> =
            Arc::new(super::storage::InMemorySettingsStorage::default());
        let content = serde_json::to_string_pretty(&initial).unwrap_or_default();
        storage
            .with_lock(SettingsScope::Global, &mut |current| {
                let _ = current;
                Some(content.clone())
            })
            .ok();
        Self::from_storage(storage)
    }

    /// Effective (global + project) settings.
    pub fn settings(&self) -> &Settings {
        &self.merged
    }

    pub fn global_settings(&self) -> &Settings {
        &self.global
    }

    pub fn project_settings(&self) -> &Settings {
        &self.project
    }

    pub fn errors(&self) -> &[SettingsError] {
        &self.errors
    }

    /// Take and clear the recorded settings errors (warnings are printed once
    /// by the CLI commands that surface them).
    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    /// Reload both scopes from storage.
    pub fn reload(&mut self) -> Result<()> {
        let mut errors = std::mem::take(&mut self.errors);
        let (global, global_load_error) =
            load_scope(self.storage.as_ref(), SettingsScope::Global, &mut errors);
        let (project, project_load_error) =
            load_scope(self.storage.as_ref(), SettingsScope::Project, &mut errors);
        self.global = global;
        self.project = project;
        self.global_load_error = global_load_error;
        self.project_load_error = project_load_error;
        self.errors = errors;
        self.merged = deep_merge(&self.global, &self.project);
        Ok(())
    }

    /// Runtime overrides layered on top (CLI flags); not persisted.
    pub fn apply_overrides(&mut self, overrides: &Settings) {
        self.merged = deep_merge(&self.merged, overrides);
        self.runtime_overrides = deep_merge(&self.runtime_overrides, overrides);
    }

    // -- persisted setters --------------------------------------------------

    pub fn set_default_provider(&mut self, provider: String) -> Result<()> {
        self.global.default_provider = Some(provider);
        self.save_global()
    }

    pub fn set_default_model(&mut self, model: String) -> Result<()> {
        self.global.default_model = Some(model);
        self.save_global()
    }

    pub fn set_default_model_and_provider(
        &mut self,
        provider: String,
        model: String,
    ) -> Result<()> {
        self.global.default_provider = Some(provider.clone());
        self.global.default_model = Some(model.clone());
        self.record_model_use(&provider, &model);
        self.save_global()
    }

    /// Record a model use at the front of `recentModels` (capped at 20).
    pub fn record_model_use(&mut self, provider: &str, model: &str) {
        let key = format!("{provider}/{model}");
        let mut recent: Vec<String> = vec![key];
        for existing in self.global.recent_models.iter().flatten() {
            if existing != &recent[0] {
                recent.push(existing.clone());
            }
        }
        recent.truncate(RECENT_MODELS_LIMIT);
        self.global.recent_models = Some(recent);
    }

    pub fn set_steering_mode(&mut self, mode: QueueModeSetting) -> Result<()> {
        self.global.steering_mode = Some(mode);
        self.save_global()
    }

    pub fn set_follow_up_mode(&mut self, mode: QueueModeSetting) -> Result<()> {
        self.global.follow_up_mode = Some(mode);
        self.save_global()
    }

    pub fn set_theme(&mut self, theme: String) -> Result<()> {
        self.global.theme = Some(theme);
        self.save_global()
    }

    pub fn set_update_channel(&mut self, channel: UpdateChannel) -> Result<()> {
        self.global.update_channel = Some(channel);
        self.save_global()
    }

    pub fn set_default_thinking_level(&mut self, level: ThinkingLevelSetting) -> Result<()> {
        self.global.default_thinking_level = Some(level);
        self.save_global()
    }

    pub fn set_transport(&mut self, transport: TransportSetting) -> Result<()> {
        self.global.transport = Some(transport);
        self.save_global()
    }

    pub fn set_rlm_max_depth(&mut self, depth: u64) -> Result<()> {
        self.global.rlm_max_depth = Some(depth);
        self.save_global()
    }

    pub fn set_telemetry_enabled(&mut self, enabled: bool) -> Result<()> {
        let telemetry = self.global.telemetry.get_or_insert_with(Default::default);
        telemetry.enabled = Some(enabled);
        self.save_global()
    }

    pub fn set_telemetry_notice_shown(&mut self, shown: bool) -> Result<()> {
        let telemetry = self.global.telemetry.get_or_insert_with(Default::default);
        telemetry.notice_shown = Some(shown);
        self.save_global()
    }

    pub fn set_onboarding_shown(&mut self, shown: bool) -> Result<()> {
        self.global.onboarding_shown = Some(shown);
        self.save_global()
    }

    pub fn set_onboarding_completed(&mut self, completed: bool) -> Result<()> {
        self.global.onboarding_completed = Some(completed);
        self.save_global()
    }

    /// Replace the `packages` array in the global settings file.
    pub fn set_packages(&mut self, packages: Vec<serde_json::Value>) {
        self.global.packages = Some(packages.clone());
        self.persist_scope_field(
            SettingsScope::Global,
            "packages",
            serde_json::Value::Array(packages),
        );
        self.merged = deep_merge(&self.global, &self.project);
    }

    /// Replace the `packages` array in the project settings file.
    pub fn set_project_packages(&mut self, packages: Vec<serde_json::Value>) {
        self.project.packages = Some(packages.clone());
        self.persist_scope_field(
            SettingsScope::Project,
            "packages",
            serde_json::Value::Array(packages),
        );
        self.merged = deep_merge(&self.global, &self.project);
    }

    /// Write one field into a scope's file, merging with the current on-disk
    /// document so concurrently-added fields survive. Settings failures are
    /// recorded as warnings, never thrown (the TS save contract).
    fn persist_scope_field(&mut self, scope: SettingsScope, field: &str, value: serde_json::Value) {
        let load_error = match scope {
            SettingsScope::Global => self.global_load_error.clone(),
            SettingsScope::Project => self.project_load_error.clone(),
        };
        if let Some(message) = load_error {
            let label = match scope {
                SettingsScope::Global => "Global",
                SettingsScope::Project => "Project",
            };
            self.errors.push(SettingsError {
                scope,
                message: format!(
                    "{label} settings not saved: settings file failed to parse: {message}"
                ),
            });
            return;
        }
        let result = self.storage.with_lock(scope, &mut |current| {
            let mut map: serde_json::Map<String, serde_json::Value> = current
                .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
                .and_then(|value| match value {
                    serde_json::Value::Object(mut map) => {
                        super::merge::migrate(&mut map);
                        Some(map)
                    }
                    _ => None,
                })
                .unwrap_or_default();
            map.insert(field.to_string(), value.clone());
            serde_json::to_string_pretty(&serde_json::Value::Object(map)).ok()
        });
        if let Err(error) = result {
            self.errors.push(SettingsError {
                scope,
                message: error.to_string(),
            });
        }
    }

    // -- getters with TS semantics ------------------------------------------

    pub fn get_default_provider(&self) -> Option<&str> {
        self.merged.default_provider.as_deref()
    }

    pub fn get_default_model(&self) -> Option<&str> {
        self.merged.default_model.as_deref()
    }

    /// Model for `rlm.spawn` without a pinned model; unset inherits parent.
    pub fn get_subagent_default_model(&self) -> Option<String> {
        self.merged
            .subagent_default_model
            .as_ref()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
    }

    pub fn get_auxiliary_model(&self) -> Option<&str> {
        self.merged.auxiliary_model.as_deref()
    }

    pub fn get_recent_models(&self) -> Vec<String> {
        self.merged.recent_models.clone().unwrap_or_default()
    }

    pub fn get_steering_mode(&self) -> QueueModeSetting {
        self.merged
            .steering_mode
            .unwrap_or(QueueModeSetting::OneAtATime)
    }

    pub fn get_follow_up_mode(&self) -> QueueModeSetting {
        self.merged
            .follow_up_mode
            .unwrap_or(QueueModeSetting::OneAtATime)
    }

    pub fn get_theme(&self) -> Option<&str> {
        self.merged.theme.as_deref()
    }

    /// Global-only read of the two known values; anything else is unset.
    pub fn get_update_channel(&self) -> Option<UpdateChannel> {
        self.global.update_channel
    }

    pub fn get_default_thinking_level(&self) -> Option<ThinkingLevelSetting> {
        self.merged.default_thinking_level
    }

    pub fn get_rlm_max_depth(&self) -> Option<u64> {
        self.global.rlm_max_depth
    }

    /// `number | "off" | "none"` -> finite minutes or Off; malformed falls
    /// back to the default (90).
    pub fn get_idle_eviction(&self) -> IdleEviction {
        match &self.global.idle_eviction_minutes {
            Some(serde_json::Value::String(text)) if text == "off" || text == "none" => {
                IdleEviction::Off
            }
            Some(serde_json::Value::Number(number)) => {
                if let Some(minutes) = number.as_u64() {
                    if minutes > 0 {
                        return IdleEviction::Minutes(minutes);
                    }
                }
                IdleEviction::Minutes(DEFAULT_IDLE_EVICTION_MINUTES)
            }
            _ => IdleEviction::Minutes(DEFAULT_IDLE_EVICTION_MINUTES),
        }
    }

    pub fn get_transport(&self) -> TransportSetting {
        self.merged.transport.unwrap_or(TransportSetting::Auto)
    }

    /// Telemetry is enabled only when every scope says so (default true).
    pub fn get_telemetry_enabled(&self) -> bool {
        [
            self.global.telemetry.as_ref(),
            self.project.telemetry.as_ref(),
            self.runtime_overrides.telemetry.as_ref(),
        ]
        .iter()
        .all(|scope| scope.and_then(|t| t.enabled).unwrap_or(true))
    }

    pub fn get_telemetry_notice_shown(&self) -> bool {
        self.runtime_overrides
            .telemetry
            .as_ref()
            .and_then(|t| t.notice_shown)
            .or_else(|| self.global.telemetry.as_ref().and_then(|t| t.notice_shown))
            .unwrap_or(false)
    }

    pub fn get_session_dir(&self) -> Option<std::path::PathBuf> {
        let session_dir = self.merged.session_dir.as_ref()?;
        let home = std::env::var("HOME").ok()?;
        Some(if session_dir == "~" {
            home.into()
        } else if let Some(rest) = session_dir.strip_prefix("~/") {
            std::path::PathBuf::from(home).join(rest)
        } else {
            session_dir.into()
        })
    }

    // -- persistence ---------------------------------------------------------

    /// Write the global scope back (project scope is host-written, not
    /// user-set in this port), then re-derive the effective settings.
    fn save_global(&mut self) -> Result<()> {
        let content = serde_json::to_string_pretty(&self.global)?;
        self.storage
            .with_lock(SettingsScope::Global, &mut |current| {
                let _ = current;
                Some(content.clone())
            })?;
        self.merged = deep_merge(&self.global, &self.project);
        Ok(())
    }
}

/// Resolved `idleEvictionMinutes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEviction {
    Minutes(u64),
    Off,
}

fn load_scope(
    storage: &dyn SettingsStorage,
    scope: SettingsScope,
    errors: &mut Vec<SettingsError>,
) -> (Settings, Option<String>) {
    let mut content: Option<String> = None;
    let mut load_error: Option<String> = None;
    let result = storage.with_lock(scope, &mut |current| {
        content = current;
        None
    });
    if let Err(error) = result {
        errors.push(SettingsError {
            scope,
            message: error.to_string(),
        });
        return (Settings::default(), Some(error.to_string()));
    }
    let Some(content) = content else {
        return (Settings::default(), None);
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(error) => {
            load_error = Some(error.to_string());
            serde_json::Value::Null
        }
    };
    if let Some(message) = load_error {
        errors.push(SettingsError {
            scope,
            message: message.clone(),
        });
        return (Settings::default(), Some(message));
    }
    // Migrate the raw document, then load leniently.
    let migrated = match value {
        serde_json::Value::Object(mut map) => {
            migrate(&mut map);
            serde_json::Value::Object(map)
        }
        other => other,
    };
    (from_value_lenient(&migrated), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_loads_merges_and_saves() {
        let mut manager = SettingsManager::in_memory(Settings::default());
        manager
            .set_default_model_and_provider("prime-inference".into(), "z-ai/glm-5.3".into())
            .unwrap();
        assert_eq!(manager.get_default_provider(), Some("prime-inference"));
        assert_eq!(manager.get_default_model(), Some("z-ai/glm-5.3"));
        assert_eq!(
            manager.get_recent_models(),
            vec!["prime-inference/z-ai/glm-5.3".to_string()]
        );
        manager.reload().unwrap();
        assert_eq!(manager.get_default_model(), Some("z-ai/glm-5.3"));
    }

    #[test]
    fn migrations_apply_on_load() {
        let storage: Arc<dyn SettingsStorage> =
            Arc::new(super::super::storage::InMemorySettingsStorage::default());
        storage
            .with_lock(SettingsScope::Global, &mut |_| {
                Some(r#"{ "queueMode": "all", "telemetry": true }"#.into())
            })
            .unwrap();
        let manager = SettingsManager::from_storage(storage);
        assert_eq!(manager.get_steering_mode(), QueueModeSetting::All);
        assert!(manager.get_telemetry_enabled());
    }

    #[test]
    fn wrong_typed_fields_never_fail_load() {
        let storage: Arc<dyn SettingsStorage> =
            Arc::new(super::super::storage::InMemorySettingsStorage::default());
        storage
            .with_lock(SettingsScope::Global, &mut |_| {
                Some(r#"{ "defaultProvider": 42, "theme": "prime" }"#.into())
            })
            .unwrap();
        let manager = SettingsManager::from_storage(storage);
        assert_eq!(manager.get_default_provider(), None);
        assert_eq!(manager.get_theme(), Some("prime"));
    }

    #[test]
    fn idle_eviction_semantics() {
        let mut manager = SettingsManager::in_memory(Settings::default());
        assert_eq!(
            manager.get_idle_eviction(),
            IdleEviction::Minutes(DEFAULT_IDLE_EVICTION_MINUTES)
        );
        manager.global.idle_eviction_minutes = Some(serde_json::json!("off"));
        assert_eq!(manager.get_idle_eviction(), IdleEviction::Off);
        manager.global.idle_eviction_minutes = Some(serde_json::json!(0));
        assert_eq!(
            manager.get_idle_eviction(),
            IdleEviction::Minutes(DEFAULT_IDLE_EVICTION_MINUTES)
        );
        manager.global.idle_eviction_minutes = Some(serde_json::json!(45));
        assert_eq!(manager.get_idle_eviction(), IdleEviction::Minutes(45));
    }
}
