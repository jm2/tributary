//! Protected Last.fm credential values and native-vault persistence.
//!
//! The production store has deliberately no plaintext fallback. A missing,
//! locked, denied, or unavailable platform vault is a closed failure that the
//! caller must surface to the user. Callers should execute these synchronous
//! vault operations on a dedicated blocking thread.

use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "io.github.tributary.Tributary.lastfm";
const ACCOUNT: &str = "session";
const ENCODING_VERSION: u8 = 1;
const ACCOUNT_BINDING_DOMAIN: &[u8] = b"tributary:lastfm-account-binding:v1\0";
const MAX_USERNAME_BYTES: usize = 512;
const SESSION_KEY_BYTES: usize = 32;

/// An in-memory secret that is redacted from formatting and wiped on drop.
#[derive(Clone, Eq, PartialEq)]
pub struct ProtectedString(String);

impl ProtectedString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProtectedString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProtectedString([REDACTED])")
    }
}

impl Drop for ProtectedString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// A durable Last.fm web-service session.
#[derive(Clone, Eq, PartialEq)]
pub struct StoredSession {
    account_id: [u8; 16],
    username: String,
    key: ProtectedString,
}

impl StoredSession {
    pub fn new(username: impl Into<String>, key: ProtectedString) -> Result<Self, CredentialError> {
        let username = username.into();
        validate_username(&username)?;
        validate_session_key(key.expose())?;
        Ok(Self {
            account_id: *uuid::Uuid::new_v4().as_bytes(),
            username,
            key,
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn key(&self) -> &ProtectedString {
        &self.key
    }

    /// Derive the only account identity permitted outside the vault record.
    pub fn account_binding(&self) -> LastFmAccountBinding {
        let mut hasher = Sha256::new();
        hasher.update(ACCOUNT_BINDING_DOMAIN);
        hasher.update(self.account_id);
        LastFmAccountBinding(hasher.finalize().into())
    }

    /// Replace a revoked session only when Last.fm returns the exact account
    /// name bytes, preserving the opaque account identity and queue binding.
    pub fn reauthorized(
        &self,
        username: impl Into<String>,
        key: ProtectedString,
    ) -> Result<Self, CredentialError> {
        let username = username.into();
        validate_username(&username)?;
        validate_session_key(key.expose())?;
        if username != self.username {
            return Err(CredentialError::AccountMismatch);
        }
        Ok(Self {
            account_id: self.account_id,
            username,
            key,
        })
    }
}

impl fmt::Debug for StoredSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredSession([REDACTED])")
    }
}

impl Drop for StoredSession {
    fn drop(&mut self) {
        self.account_id.zeroize();
        self.username.zeroize();
    }
}

/// One-way, domain-separated identity safe to use as a queue binding.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct LastFmAccountBinding([u8; 32]);

impl LastFmAccountBinding {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for LastFmAccountBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmAccountBinding([REDACTED])")
    }
}

/// Fixed, secret-free native credential-store failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialError {
    #[error("protected credential store is unavailable")]
    Unavailable,
    #[error("protected credential store contains invalid session data")]
    InvalidData,
    #[error("Last.fm authorization belongs to a different account")]
    AccountMismatch,
}

/// Storage boundary for a Last.fm session.
///
/// Implementations must provide protected storage or fail. In particular,
/// they must not silently substitute a settings file or environment variable.
pub trait SessionCredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError>;
    fn save(&self, session: &StoredSession) -> Result<(), CredentialError>;
    fn delete(&self) -> Result<(), CredentialError>;
}

/// Native protected credential storage selected by `keyring` for the OS.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsSessionCredentialStore;

impl SessionCredentialStore for OsSessionCredentialStore {
    fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
        let entry = native_entry()?;
        match entry.get_secret() {
            Ok(bytes) => decode_session(&Zeroizing::new(bytes)).map(Some),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }

    fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
        let encoded = encode_session(session)?;
        native_entry()?
            .set_secret(&encoded)
            .map_err(|_| CredentialError::Unavailable)
    }

    fn delete(&self) -> Result<(), CredentialError> {
        let entry = native_entry()?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }
}

fn native_entry() -> Result<keyring_core::Entry, CredentialError> {
    // Construct the selected native store explicitly for every operation.
    // Failed Secret Service/Keychain/Credential Manager initialization is
    // therefore retryable instead of poisoning a process-global initializer.
    native_store()?
        .build(SERVICE, ACCOUNT, None)
        .map_err(|_| CredentialError::Unavailable)
}

#[cfg(target_os = "linux")]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>, CredentialError> {
    let store: Arc<keyring_core::CredentialStore> = zbus_secret_service_keyring_store::Store::new()
        .map_err(|_| CredentialError::Unavailable)?;
    Ok(store)
}

#[cfg(target_os = "windows")]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>, CredentialError> {
    let store: Arc<keyring_core::CredentialStore> =
        windows_native_keyring_store::Store::new().map_err(|_| CredentialError::Unavailable)?;
    Ok(store)
}

#[cfg(target_os = "macos")]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>, CredentialError> {
    let store: Arc<keyring_core::CredentialStore> =
        apple_native_keyring_store::keychain::Store::new()
            .map_err(|_| CredentialError::Unavailable)?;
    Ok(store)
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn native_store() -> Result<Arc<keyring_core::CredentialStore>, CredentialError> {
    Err(CredentialError::Unavailable)
}

fn encode_session(session: &StoredSession) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
    let username = session.username.as_bytes();
    let key = session.key.expose().as_bytes();
    validate_username(&session.username)?;
    validate_session_key(session.key.expose())?;
    let username_length =
        u16::try_from(username.len()).map_err(|_| CredentialError::InvalidData)?;
    // Keep the complete plaintext vault payload behind an RAII wipe from its
    // first allocation. In particular, `save` may return early when native
    // entry construction fails, and panics or future error branches must not
    // be able to bypass the cleanup.
    let mut encoded = Zeroizing::new(Vec::new());
    encoded
        .try_reserve(19 + username.len() + key.len())
        .map_err(|_| CredentialError::InvalidData)?;
    encoded.push(ENCODING_VERSION);
    encoded.extend_from_slice(&session.account_id);
    encoded.extend_from_slice(&username_length.to_be_bytes());
    encoded.extend_from_slice(username);
    encoded.extend_from_slice(key);
    Ok(encoded)
}

fn decode_session(encoded: &[u8]) -> Result<StoredSession, CredentialError> {
    let (&version, remainder) = encoded.split_first().ok_or(CredentialError::InvalidData)?;
    if version != ENCODING_VERSION || remainder.len() < 18 {
        return Err(CredentialError::InvalidData);
    }
    let mut account_id = [0_u8; 16];
    account_id.copy_from_slice(&remainder[..16]);
    let account_uuid = uuid::Uuid::from_bytes(account_id);
    if account_uuid.get_variant() != uuid::Variant::RFC4122
        || account_uuid.get_version() != Some(uuid::Version::Random)
    {
        return Err(CredentialError::InvalidData);
    }
    let username_length = usize::from(u16::from_be_bytes([remainder[16], remainder[17]]));
    let payload = &remainder[18..];
    if username_length > payload.len() {
        return Err(CredentialError::InvalidData);
    }
    let (username, key) = payload.split_at(username_length);
    validate_component(username, MAX_USERNAME_BYTES)?;
    let username = std::str::from_utf8(username).map_err(|_| CredentialError::InvalidData)?;
    let key = std::str::from_utf8(key).map_err(|_| CredentialError::InvalidData)?;
    validate_username(username)?;
    validate_session_key(key)?;
    Ok(StoredSession {
        account_id,
        username: username.to_string(),
        key: ProtectedString::new(key),
    })
}

fn validate_component(value: &[u8], maximum: usize) -> Result<(), CredentialError> {
    if value.is_empty() || value.len() > maximum || value.contains(&0) {
        Err(CredentialError::InvalidData)
    } else {
        Ok(())
    }
}

fn validate_username(value: &str) -> Result<(), CredentialError> {
    validate_component(value.as_bytes(), MAX_USERNAME_BYTES)?;
    if !value.chars().any(|character| !character.is_whitespace())
        || value.chars().any(char::is_control)
    {
        Err(CredentialError::InvalidData)
    } else {
        Ok(())
    }
}

fn validate_session_key(value: &str) -> Result<(), CredentialError> {
    if value.len() == SESSION_KEY_BYTES && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(CredentialError::InvalidData)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{
        decode_session, encode_session, CredentialError, ProtectedString, SessionCredentialStore,
        StoredSession, ENCODING_VERSION,
    };

    const SESSION_KEY: &str = "0123456789abcdef0123456789abcdef";
    const RENEWED_SESSION_KEY: &str = "abcdef0123456789abcdef0123456789";

    #[derive(Default)]
    struct MemoryStore(Mutex<Option<StoredSession>>);

    impl SessionCredentialStore for MemoryStore {
        fn load(&self) -> Result<Option<StoredSession>, CredentialError> {
            self.0
                .lock()
                .map_err(|_| CredentialError::Unavailable)
                .map(|session| session.clone())
        }

        fn save(&self, session: &StoredSession) -> Result<(), CredentialError> {
            *self.0.lock().map_err(|_| CredentialError::Unavailable)? = Some(session.clone());
            Ok(())
        }

        fn delete(&self) -> Result<(), CredentialError> {
            *self.0.lock().map_err(|_| CredentialError::Unavailable)? = None;
            Ok(())
        }
    }

    fn session() -> StoredSession {
        StoredSession::new("listener", ProtectedString::new(SESSION_KEY)).expect("valid session")
    }

    #[test]
    fn protected_values_are_always_redacted() {
        let secret = ProtectedString::new("do-not-print-this");
        let session = StoredSession::new("private-user", ProtectedString::new(SESSION_KEY))
            .expect("valid session");
        let rendered = format!("{secret:?} {session:?}");
        assert!(!rendered.contains("do-not-print-this"));
        assert!(!rendered.contains("private-user"));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn versioned_encoding_round_trips_without_plaintext_fallback() {
        let original = session();
        let encoded = encode_session(&original).expect("session encodes");
        assert_eq!(encoded[0], ENCODING_VERSION);
        let decoded = decode_session(&encoded).expect("session decodes");
        assert_eq!(decoded.username(), "listener");
        assert_eq!(decoded.key().expose(), SESSION_KEY);
        assert_eq!(decoded.account_binding(), original.account_binding());
    }

    #[test]
    fn encoded_vault_payload_guarantees_drop_zeroization() {
        fn require_drop_zeroization<T: zeroize::ZeroizeOnDrop>(_: &T) {}

        let encoded = encode_session(&session()).expect("session encodes");
        require_drop_zeroization(&encoded);
        assert!(encoded.ends_with(SESSION_KEY.as_bytes()));
    }

    #[test]
    fn malformed_or_oversized_encodings_fail_closed() {
        for fixture in [
            Vec::new(),
            vec![ENCODING_VERSION + 1, 0, 0],
            vec![ENCODING_VERSION; 18],
        ] {
            assert_eq!(decode_session(&fixture), Err(CredentialError::InvalidData));
        }

        let mut invalid_identity = encode_session(&session()).expect("fixture encodes");
        invalid_identity[1..17].fill(0);
        assert_eq!(
            decode_session(&invalid_identity),
            Err(CredentialError::InvalidData)
        );
        assert_eq!(
            StoredSession::new("line\nbreak", ProtectedString::new(SESSION_KEY)),
            Err(CredentialError::InvalidData)
        );
        assert_eq!(
            StoredSession::new(" \t ", ProtectedString::new(SESSION_KEY)),
            Err(CredentialError::InvalidData)
        );
        for invalid_key in [
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef\n123456789abcdef",
        ] {
            assert_eq!(
                StoredSession::new("listener", ProtectedString::new(invalid_key)),
                Err(CredentialError::InvalidData)
            );
        }
    }

    #[test]
    fn trait_supports_isolated_test_stores() {
        let store = MemoryStore::default();
        assert_eq!(store.load().expect("empty store loads"), None);
        let expected = session();
        store.save(&expected).expect("session saves");
        assert_eq!(
            store
                .load()
                .expect("session loads")
                .expect("stored session"),
            expected
        );
        store.delete().expect("session deletes");
        assert_eq!(store.load().expect("deleted store loads"), None);
    }

    #[test]
    fn credential_errors_never_retain_secret_context() {
        for error in [
            CredentialError::Unavailable,
            CredentialError::InvalidData,
            CredentialError::AccountMismatch,
        ] {
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("private-user"));
        }
    }

    #[test]
    fn account_binding_is_stable_one_way_and_reauthorization_is_exact() {
        let original = session();
        let binding = original.account_binding();
        assert_eq!(binding, original.account_binding());
        assert_eq!(format!("{binding:?}"), "LastFmAccountBinding([REDACTED])");
        assert_eq!(binding.as_bytes().len(), 32);

        let renewed = original
            .reauthorized("listener", ProtectedString::new(RENEWED_SESSION_KEY))
            .expect("exact account renews");
        assert_eq!(renewed.account_binding(), binding);
        assert_eq!(renewed.key().expose(), RENEWED_SESSION_KEY);
        assert_eq!(
            original.reauthorized("Listener", ProtectedString::new(RENEWED_SESSION_KEY)),
            Err(CredentialError::AccountMismatch)
        );
        assert_ne!(session().account_binding(), binding);
    }
}
