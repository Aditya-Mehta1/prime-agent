//! Package manager subsystem: install/remove/list/update of `npm:`, git, and
//! local-dir package sources against the settings store, plus the
//! configured-npm/git child-process flows.
//!
//! Non-goals (follow-up specs in `docs/parity-checklist.md`): resource
//! resolution for sessions (`resolve()` -> skills/prompts/themes/extension
//! paths), the extension runner, and Prime Agent self-updates.

mod git;
mod manager;
mod npm;
mod process;
mod source;
mod update;

#[cfg(test)]
mod tests;

pub use manager::{
    ConfiguredPackage, PackageManager, PackageUpdate, ProgressAction, ProgressEvent,
    ProgressEventKind, UserOrProject,
};
pub use source::{GitSource, LocalSource, NpmSource, ParsedSource, SourceScope};

use std::path::PathBuf;

/// The TS `CONFIG_DIR_NAME` (project-local settings/packages root).
pub use crate::settings::CONFIG_DIR_NAME;

/// Network probe timeout for npm/git operations (10s).
pub(crate) use npm::NETWORK_TIMEOUT_MS;

/// True when `PI_OFFLINE` disables all package network operations.
pub(crate) fn is_offline_mode_enabled() -> bool {
    std::env::var("PI_OFFLINE")
        .map(|value| {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}

/// Stable temporary directory for resolve-only package installs:
/// `/tmp/pi-extensions/<prefix>/<hash8>/<suffix?>` (the hash keys on
/// prefix+suffix so the same source always maps to one checkout).
pub(crate) fn temporary_dir(prefix: &str, suffix: Option<&str>) -> PathBuf {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(format!("{prefix}-{}", suffix.unwrap_or_default()).as_bytes());
    let digest = hasher.finalize();
    let hash: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    std::env::temp_dir()
        .join("pi-extensions")
        .join(prefix)
        .join(&hash)
        .join(suffix.unwrap_or_default())
}
