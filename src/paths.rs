//! User-state path resolution.
//!
//! Production resolves the application data root through the platform's
//! native directory APIs (`dirs::data_dir()`). Process-isolated tests must
//! redirect that root into a private sandbox so the child can neither read
//! nor write the real user's state.
//!
//! The redirect is a test-scoped environment override honored *before* the
//! platform lookup. That ordering is what makes it work on every supported
//! platform: on Windows `dirs` resolves `FOLDERID_RoamingAppData` through
//! `SHGetKnownFolderPath` and ignores `HOME` and the `XDG_*` variables, so
//! redirecting only those variables does not isolate the child there.

/// Environment variable used only by process-isolated tests to redirect
/// [`data_dir`] into a private sandbox.
#[cfg(test)]
pub const TEST_USER_STATE_DIR_ENV: &str = "TRIBUTARY_TEST_USER_STATE_DIR";

/// Resolve the Tributary application data root (`<data_dir>`).
///
/// In test builds a non-empty `TEST_USER_STATE_DIR_ENV` takes precedence
/// over the platform directory, so every production persistence path
/// (volume, database, settings, server/output/config files) lands inside the
/// isolated child's sandbox. The override is compiled out of non-test builds,
/// where this always resolves `dirs::data_dir()`.
pub fn data_dir() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(dir) = std::env::var_os(TEST_USER_STATE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(std::path::PathBuf::from(dir));
    }
    dirs::data_dir()
}

/// Resolve the user cache root (`<cache_dir>`).
///
/// In test builds a non-empty `TEST_USER_STATE_DIR_ENV` redirects it to a
/// `cache` folder inside that sandbox, as it does for [`data_dir`].
pub fn cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(dir) = std::env::var_os(TEST_USER_STATE_DIR_ENV).filter(|value| !value.is_empty()) {
        return Some(std::path::PathBuf::from(dir).join("cache"));
    }
    dirs::cache_dir()
}
