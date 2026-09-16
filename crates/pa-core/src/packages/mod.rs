//! Package manager subsystem: install/remove/list/update of `npm:`, git, and
//! local-dir package sources against the settings store, plus the
//! configured-npm/git child-process flows.
//!
//! Session resource resolution lives here as well: `PackageManager::resolve`
//! produces the ranked skill/prompt/theme/extension paths sessions consume.
//!
//! Non-goals (follow-up specs in `docs/parity-checklist.md`): the extension
//! *runner* (loading/executing extension modules) and Prime Agent
//! self-updates.

mod git;
mod manager;
mod npm;
mod process;
pub(crate) mod resolve;
mod source;
mod update;

#[cfg(test)]
mod tests;

pub use manager::{
    BundledSkillsDir, ConfiguredPackage, PackageManager, PackageManagerOptions, PackageUpdate,
    ProgressAction, ProgressEvent, ProgressEventKind, UserOrProject,
};
pub use resolve::{
    MetadataSource, MissingSourceAction, PathMetadata, ResolveExtensionOptions, ResolvedPaths,
    ResolvedResource, ResourceOrigin, ResourceType,
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

/// The directory of built-in skills shipped with the package: `skills/`
/// next to the executable (the packaged layout; TS `getBundledSkillsDir`).
pub(crate) fn get_bundled_skills_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("skills")))
        .unwrap_or_else(|| PathBuf::from("skills"))
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
