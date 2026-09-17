//! Materialization of the bundled extension host script (design doc
//! §2.2: shipped inside the binary, written under `<agentDir>/extension-host/`
//! at first use, content-addressed so upgrades never mutate a file a running
//! sidecar may still read).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// The stage-1 host script bundle (`host_script.mjs`): the protocol-1 peer
/// (handshake, ping, event no-op, shutdown). Stage 2 rewrites this bundle to
/// load extension modules (vendored jiti/static + the pi API shim).
const HOST_SCRIPT: &str = include_str!("host_script.mjs");

/// Write the bundled host script under `<agent_dir>/extension-host/`, named
/// by the script's sha256. Already-materialized builds are a cache hit; the
/// temp-file + rename keeps a partially written script unobservable, and a
/// concurrent materializer that loses the rename finds the winner in place.
pub(crate) fn materialize_host_script(agent_dir: &Path) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(HOST_SCRIPT.as_bytes());
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let dir = agent_dir.join("extension-host");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("host-{digest}.mjs"));
    if path.is_file() {
        return Ok(path);
    }
    let temp = dir.join(format!(".host-{digest}.{}.tmp", std::process::id()));
    std::fs::write(&temp, HOST_SCRIPT).with_context(|| format!("writing {}", temp.display()))?;
    if std::fs::rename(&temp, &path).is_err() {
        // Lost the race to another materializer: drop our temp copy, the
        // winner's identical content is already in place.
        let _ = std::fs::remove_file(&temp);
    }
    if !path.is_file() {
        return Err(anyhow::anyhow!(
            "extension host script did not materialize: {}",
            path.display()
        ));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_under_extension_host_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let first = materialize_host_script(temp.path()).unwrap();
        assert_eq!(
            first.parent().unwrap().file_name().unwrap(),
            "extension-host"
        );
        let second = materialize_host_script(temp.path()).unwrap();
        assert_eq!(first, second, "content-addressed name must be stable");
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("host-") && name.ends_with(".mjs"));
    }

    #[test]
    fn materialized_content_matches_the_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let path = materialize_host_script(temp.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, HOST_SCRIPT);
    }

    #[test]
    fn content_address_excludes_leftover_temp_files() {
        let temp = tempfile::tempdir().unwrap();
        materialize_host_script(temp.path()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(temp.path().join("extension-host"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "no temp files survive: {entries:?}");
    }
}
