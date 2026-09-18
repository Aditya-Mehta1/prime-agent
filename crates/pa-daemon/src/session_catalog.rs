//! Saved-session catalog resolve (port of the catalog `resolve` command in
//! `modes/daemon/daemon-catalog-process.ts`): the selector-to-session-file
//! lookup the supervisor uses when a client command addresses a session
//! that is not resident. Local (cwd-scoped) matches win over global ones;
//! an ambiguous selector fails with the TS error so callers can tell it
//! apart from an unknown-target miss.

use std::path::Path;

use anyhow::{anyhow, Result};

use crate::session_store::{list_sessions, SessionInfo};

/// One selector match against saved sessions (TS
/// `resolveCatalogSessionMatch`): the session id is addressable by prefix,
/// the name only exactly. More than one match is ambiguous.
fn catalog_session_match<'a>(
    sessions: impl IntoIterator<Item = &'a SessionInfo>,
    selector: &str,
) -> Result<Option<&'a SessionInfo>> {
    let matches: Vec<&SessionInfo> = sessions
        .into_iter()
        .filter(|info| info.id.starts_with(selector) || info.name.as_deref() == Some(selector))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(anyhow!("Ambiguous session selector \"{selector}\"")),
    }
}

/// Resolve one selector to a saved session (TS catalog `resolve`):
/// cwd-scoped sessions first, the whole catalog second. The returned info
/// carries the session's own cwd for the wake create. Misses are `None`,
/// so the caller answers with its unknown-target error, not a catalog one.
pub(crate) fn resolve_saved_session(
    sessions_dir: &Path,
    selector: &str,
    cwd: &str,
) -> Result<Option<SessionInfo>> {
    let all = list_sessions(sessions_dir);
    let local = catalog_session_match(all.iter().filter(|info| info.cwd == cwd), selector)?;
    if let Some(info) = local {
        return Ok(Some(info.clone()));
    }
    let global = catalog_session_match(all.iter(), selector)?;
    Ok(global.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::{session_file_name, SessionFile};
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pa-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(dir: &Path, cwd: &str, name: Option<&str>) -> (String, PathBuf) {
        let mut session = SessionFile::create(cwd, None, 0);
        if let Some(name) = name {
            session.append_session_info(name);
        }
        session.append_message(serde_json::json!({
            "role": "user", "content": "hi", "timestamp": 1u64
        }));
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        session.rewrite().unwrap();
        (session.session_id().to_string(), path)
    }

    #[test]
    fn resolves_by_id_prefix_and_exact_name() {
        let dir = temp_dir();
        let (id, _path) = write_session(&dir, "/work/a", Some("alpha"));
        let found = resolve_saved_session(&dir, &id[..8], "/work/a")
            .unwrap()
            .expect("id prefix resolves");
        assert_eq!(found.id, id);
        let found = resolve_saved_session(&dir, "alpha", "/work/a")
            .unwrap()
            .expect("name resolves");
        assert_eq!(found.id, id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_scope_wins_and_global_is_the_fallback() {
        let dir = temp_dir();
        let (id, _path) = write_session(&dir, "/work/a", Some("alpha"));
        // A different cwd still resolves through the global pass.
        let found = resolve_saved_session(&dir, "alpha", "/work/other")
            .unwrap()
            .expect("global fallback");
        assert_eq!(found.id, id);
        // The local pass serves the session's own cwd.
        let found = resolve_saved_session(&dir, &id[..8], "/work/a")
            .unwrap()
            .expect("local pass");
        assert_eq!(found.cwd, "/work/a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ambiguous_selectors_carry_the_ts_error() {
        let dir = temp_dir();
        write_session(&dir, "/work/a", Some("alpha"));
        write_session(&dir, "/work/a", Some("alpha"));
        let error = resolve_saved_session(&dir, "alpha", "/work/a").unwrap_err();
        assert_eq!(error.to_string(), "Ambiguous session selector \"alpha\"");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_miss_is_none_not_an_error() {
        let dir = temp_dir();
        write_session(&dir, "/work/a", Some("alpha"));
        let found = resolve_saved_session(&dir, "ghost", "/work/a").unwrap();
        assert!(found.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
