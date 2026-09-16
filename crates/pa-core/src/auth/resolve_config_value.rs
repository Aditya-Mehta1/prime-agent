//! Resolve config values: `!command` (cached stdout), env var, or literal.
//! Port of resolve-config-value.ts.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;

static COMMAND_RESULT_CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);

/// Resolve a config value: `!command` executes and caches; otherwise the
/// environment wins over the literal string (set-but-empty means missing).
pub fn resolve_config_value(config: &str) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return execute_command(config, command);
    }
    resolve_env_or_literal(config)
}

/// Never-cached variant used when a command key was marked stale.
pub fn resolve_config_value_uncached(config: &str) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command).ok().flatten();
    }
    resolve_env_or_literal(config)
}

/// Unset env var falls back to the literal string; set-but-empty is a missing
/// credential (never the variable name).
fn resolve_env_or_literal(config: &str) -> Option<String> {
    match std::env::var(config) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) => None,
        Err(_) => Some(config.to_string()),
    }
}

fn execute_command(cache_key: &str, command: &str) -> Option<String> {
    let mut cache = COMMAND_RESULT_CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(cached) = cache.get(cache_key) {
        return cached.clone();
    }
    let value = run_command(command).ok().flatten();
    cache.insert(cache_key.to_string(), value.clone());
    value
}

fn run_command(command: &str) -> Result<Option<String>> {
    // Hidden spawn: stdin closed, stdout captured, stderr suppressed.
    use std::process::{Command, Stdio};
    let output = Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_literal_semantics() {
        // Literal when the variable is unset.
        let key = "PA_TEST_DEFINITELY_UNSET_VAR";
        assert_eq!(resolve_env_or_literal(key), Some(key.to_string()));
    }

    #[test]
    fn command_resolution_and_cache() {
        let value = resolve_config_value("!echo resolved-value");
        assert_eq!(value.as_deref(), Some("resolved-value"));
        // Cached path returns the same value.
        assert_eq!(
            resolve_config_value("!echo resolved-value").as_deref(),
            Some("resolved-value")
        );
        // Failing command resolves to None but does not poison the cache.
        assert_eq!(resolve_config_value("!exit 1"), None);
    }
}
