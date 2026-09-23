//! Per-install client identity sent to media servers.
//!
//! Jellyfin keys sessions on `DeviceId` and, whenever it issues a token,
//! revokes the same user's other sessions for that `DeviceId`; Plex keys
//! devices on `X-Plex-Client-Identifier`. Each install therefore needs its own
//! identifier, generated once and kept in `<data_dir>/tributary/client-id`.
//! It identifies the install, not the user, and is not a credential.

use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use tracing::warn;
use uuid::Uuid;

const FILE_NAME: &str = "client-id";

/// This install's client identifier: a hyphenated UUID, which is always a
/// valid HTTP header value and needs no quoting inside Jellyfin's
/// `Authorization` parameters.
///
/// Read or created once per process. When the data directory cannot be read
/// or written, the identifier is still unique but lasts only for this process.
pub fn install_id() -> &'static str {
    static INSTALL_ID: OnceLock<String> = OnceLock::new();
    INSTALL_ID.get_or_init(|| {
        // Unit tests share the real user's data directory, so they must not
        // create the file there; they exercise `load_or_create` directly.
        if cfg!(test) {
            return Uuid::new_v4().to_string();
        }
        crate::paths::data_dir().map_or_else(
            || Uuid::new_v4().to_string(),
            |dir| load_or_create(&dir.join("tributary")),
        )
    })
}

/// Return the identifier stored in `dir`, creating it when absent or invalid.
fn load_or_create(dir: &Path) -> String {
    let path = dir.join(FILE_NAME);
    if let Some(id) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok())
    {
        return id.hyphenated().to_string();
    }

    let id = Uuid::new_v4().to_string();
    if let Err(error) = persist(dir, &path, &id) {
        warn!(
            error = %error,
            path = %path.display(),
            "Could not save the client identifier; media servers will see a new device next launch"
        );
    }
    id
}

/// Atomically replace `path` so a crash never leaves a truncated identifier.
fn persist(dir: &Path, path: &Path, id: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    writeln!(temporary, "{id}")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_is_stable_within_one_install() {
        let install = tempfile::tempdir().expect("install dir");
        let dir = install.path().join("tributary");

        let first = load_or_create(&dir);
        assert!(Uuid::parse_str(&first).is_ok());
        assert_eq!(load_or_create(&dir), first);
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE_NAME)).expect("persisted id"),
            format!("{first}\n")
        );
    }

    #[test]
    fn separate_installs_get_distinct_identifiers() {
        let first = tempfile::tempdir().expect("first install");
        let second = tempfile::tempdir().expect("second install");

        assert_ne!(load_or_create(first.path()), load_or_create(second.path()));
    }

    #[test]
    fn invalid_stored_identifier_is_replaced_and_then_kept() {
        let install = tempfile::tempdir().expect("install dir");
        let path = install.path().join(FILE_NAME);
        std::fs::write(&path, "Tributary\", Token=\"injected").expect("corrupt id");

        let replacement = load_or_create(install.path());
        assert!(Uuid::parse_str(&replacement).is_ok());
        assert_eq!(load_or_create(install.path()), replacement);
    }

    #[test]
    fn process_identifier_is_a_stable_uuid() {
        assert!(Uuid::parse_str(install_id()).is_ok());
        assert_eq!(install_id(), install_id());
        assert_ne!(install_id(), "Tributary");
    }
}
