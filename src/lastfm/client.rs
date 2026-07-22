//! Bounded Last.fm 2.0 desktop-authentication and scrobbling client.

use std::fmt;
use std::time::Duration;

use md5::{Digest, Md5};
use reqwest::header::CONTENT_TYPE;
use reqwest::{StatusCode, Url};
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;
use zeroize::{Zeroize, Zeroizing};

use crate::http_body::{read_limited, ResponseBodyError};

use super::credentials::{ProtectedString, StoredSession};

const API_ENDPOINT: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTHORIZATION_ENDPOINT: &str = "https://www.last.fm/api/auth/";
const USER_AGENT: &str = concat!("Tributary/", env!("CARGO_PKG_VERSION"));
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
// Four 1,024-byte metadata values may each expand threefold under form
// percent-encoding. Across 50 rows that is 614,400 bytes before bounded field
// names and numeric values; one MiB leaves a deliberately conservative margin.
const MAX_FORM_BODY_BYTES: usize = 1024 * 1024;
// A JSON service is allowed to represent one input byte as a six-byte
// `\uXXXX` escape. Fifty responses echoing four maximum-size fields therefore
// need 1,228,800 bytes before bounded structural overhead. Two MiB covers that
// valid worst case while still placing a small, authoritative response cap.
const MAX_RESPONSE_BODY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 1024;
const MAX_USERNAME_BYTES: usize = 512;
const EXPECTED_CREDENTIAL_BYTES: usize = 32;

/// Last.fm's documented maximum number of scrobbles in one request.
pub const MAX_SCROBBLES_PER_BATCH: usize = 50;

/// Compile-time Last.fm application identity and signing secret.
pub struct AppCredentials {
    api_key: ProtectedString,
    shared_secret: ProtectedString,
}

impl AppCredentials {
    /// Load application credentials embedded by the release build.
    ///
    /// CI and developer builds intentionally compile without these variables;
    /// attempting to enable Last.fm in such a build fails closed at runtime.
    pub(super) fn from_build() -> Result<Self, LastFmClientError> {
        let api_key = option_env!("TRIBUTARY_LASTFM_API_KEY")
            .ok_or(LastFmClientError::AppCredentialsUnavailable)?;
        let shared_secret = option_env!("TRIBUTARY_LASTFM_SHARED_SECRET")
            .ok_or(LastFmClientError::AppCredentialsUnavailable)?;
        Self::from_values(api_key, shared_secret)
            .map_err(|_| LastFmClientError::AppCredentialsUnavailable)
    }

    #[cfg(test)]
    fn for_test(api_key: &str, shared_secret: &str) -> Result<Self, LastFmClientError> {
        Self::from_values(api_key, shared_secret)
    }

    fn from_values(api_key: &str, shared_secret: &str) -> Result<Self, LastFmClientError> {
        validate_hex_credential(api_key)?;
        validate_hex_credential(shared_secret)?;
        Ok(Self {
            api_key: ProtectedString::new(api_key),
            shared_secret: ProtectedString::new(shared_secret),
        })
    }
}

impl fmt::Debug for AppCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AppCredentials([REDACTED])")
    }
}

/// Unauthorized, one-use desktop authentication token.
pub(super) struct DesktopAuthToken(ProtectedString);

impl DesktopAuthToken {
    fn from_response(mut value: SecretResponseString) -> Result<Self, LastFmClientError> {
        let value = ProtectedString::new(value.take());
        validate_hex_credential(value.expose()).map_err(|_| LastFmClientError::InvalidResponse)?;
        Ok(Self(value))
    }

    #[cfg(test)]
    pub(super) fn for_test(value: &str) -> Result<Self, LastFmClientError> {
        Self::from_response(SecretResponseString::for_test(value))
    }
}

impl fmt::Debug for DesktopAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesktopAuthToken([REDACTED])")
    }
}

/// Browser URL that carries an ephemeral desktop auth token.
pub(super) struct DesktopAuthorizationUrl(ProtectedString);

impl DesktopAuthorizationUrl {
    #[cfg(test)]
    pub(super) fn as_str(&self) -> &str {
        self.0.expose()
    }

    #[cfg(test)]
    pub(super) fn for_test(value: &str) -> Result<Self, LastFmClientError> {
        let url = Url::parse(value).map_err(|_| LastFmClientError::InvalidInput)?;
        if url.scheme() != "https"
            || url.host_str() != Some("www.last.fm")
            || url.port_or_known_default() != Some(443)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/api/auth/"
            || url.fragment().is_some()
        {
            return Err(LastFmClientError::InvalidInput);
        }
        let mut api_key = None;
        let mut token = None;
        for (name, value) in url.query_pairs() {
            let slot = match name.as_ref() {
                "api_key" => &mut api_key,
                "token" => &mut token,
                _ => return Err(LastFmClientError::InvalidInput),
            };
            if slot.replace(value.into_owned()).is_some() {
                return Err(LastFmClientError::InvalidInput);
            }
        }
        let api_key = api_key.ok_or(LastFmClientError::InvalidInput)?;
        let token = token.ok_or(LastFmClientError::InvalidInput)?;
        validate_hex_credential(&api_key)?;
        validate_hex_credential(&token)?;
        Ok(Self(ProtectedString::new(String::from(url))))
    }
}

impl fmt::Debug for DesktopAuthorizationUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesktopAuthorizationUrl([REDACTED])")
    }
}

/// Validated result of a one-shot desktop token exchange.
///
/// This deliberately is not a [`StoredSession`]: deciding whether to mint a
/// fresh opaque account identity or preserve an exact existing identity is a
/// lifecycle decision made only after account/replacement policy runs. Both
/// fields remain content-redacted, and the username is wiped if this staged
/// result is cancelled or rejected before installation.
pub(super) struct DesktopAuthorizedSession {
    username: Zeroizing<String>,
    key: Zeroizing<String>,
}

impl DesktopAuthorizedSession {
    fn from_response(response: RawSession) -> Result<Self, LastFmClientError> {
        // Protect both values before validation so malformed provider output
        // is wiped on every error path as well as after successful staging.
        let staged = Self {
            username: response.name.into_zeroizing(),
            key: response.key.into_zeroizing(),
        };
        validate_required_text(&staged.username, MAX_USERNAME_BYTES)
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        validate_hex_credential(&staged.key).map_err(|_| LastFmClientError::InvalidResponse)?;
        Ok(staged)
    }

    pub(super) fn username(&self) -> &str {
        &self.username
    }

    #[cfg(test)]
    pub(super) fn for_test(username: &str, key: &str) -> Result<Self, LastFmClientError> {
        Self::from_response(RawSession {
            name: SecretResponseString::for_test(username),
            key: SecretResponseString::for_test(key),
        })
    }

    /// Move the validated identity and key to the serialized vault installer.
    ///
    /// The caller must either create a new [`StoredSession`] or use
    /// [`StoredSession::reauthorized`] after exact-byte account comparison.
    pub(super) fn into_parts(self) -> (Zeroizing<String>, ProtectedString) {
        let Self { username, mut key } = self;
        let key = ProtectedString::new(std::mem::take(&mut *key));
        (username, key)
    }
}

impl fmt::Debug for DesktopAuthorizedSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesktopAuthorizedSession([REDACTED])")
    }
}

/// Metadata admitted to now-playing and scrobble requests.
#[derive(Clone, Eq, PartialEq)]
pub struct LastFmTrack {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub duration_seconds: u32,
}

impl fmt::Debug for LastFmTrack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LastFmTrack([REDACTED])")
    }
}

/// One completed play submitted to Last.fm.
#[derive(Clone, Eq, PartialEq)]
pub struct Scrobble {
    pub track: LastFmTrack,
    pub started_at_unix_seconds: u64,
}

impl fmt::Debug for Scrobble {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Scrobble([REDACTED])")
    }
}

/// Last.fm's disposition for an individual now-playing or scrobble item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionResult {
    Accepted { corrected: bool },
    Ignored { reason: IgnoredReason },
}

/// Documented ignored-message code, retaining no submitted metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IgnoredReason {
    Artist,
    Track,
    TimestampTooOld,
    TimestampTooNew,
    DailyLimit,
    Other(u16),
}

/// Ordered per-input results for a Last.fm batch response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrobbleBatchResult {
    pub items: Vec<SubmissionResult>,
}

/// Closed, metadata-free Last.fm protocol failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LastFmClientError {
    #[error("Last.fm application credentials are unavailable")]
    AppCredentialsUnavailable,
    #[error("Last.fm request data is invalid")]
    InvalidInput,
    #[error("Last.fm HTTP client construction failed")]
    ClientConstruction,
    #[error("Last.fm request timed out")]
    Timeout,
    #[error("Last.fm transport failed")]
    Transport,
    #[error("Last.fm returned an HTTP error")]
    HttpStatus,
    #[error("Last.fm service is temporarily unavailable")]
    ServiceUnavailable,
    #[error("Last.fm rate limit was exceeded")]
    RateLimited,
    #[error("Last.fm authentication must be renewed")]
    ReauthenticationRequired,
    #[error("Last.fm rejected the request")]
    ServiceRejected { code: u16 },
    #[error("Last.fm response exceeded its size policy")]
    BodyLimit,
    #[error("Last.fm response was invalid")]
    InvalidResponse,
}

impl LastFmClientError {
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Transport | Self::ServiceUnavailable | Self::RateLimited
        )
    }

    pub const fn requires_reauthentication(self) -> bool {
        matches!(self, Self::ReauthenticationRequired)
    }
}

#[derive(Clone, Copy)]
struct RequestPolicy {
    timeout: Duration,
    maximum_response_bytes: u64,
    maximum_form_bytes: usize,
}

/// Form parameters include session material and must be wiped on every exit,
/// including task cancellation while an HTTP request is in flight.
struct SensitiveParameters(Vec<(String, String)>);

impl Drop for SensitiveParameters {
    fn drop(&mut self) {
        for (_, value) in &mut self.0 {
            value.zeroize();
        }
    }
}

impl RequestPolicy {
    const PRODUCTION: Self = Self {
        timeout: REQUEST_TIMEOUT,
        maximum_response_bytes: MAX_RESPONSE_BODY_BYTES,
        maximum_form_bytes: MAX_FORM_BODY_BYTES,
    };
}

/// Stateless client for the exact Last.fm HTTPS API origin.
pub struct LastFmClient {
    endpoint: Url,
    http: reqwest::Client,
    credentials: AppCredentials,
    policy: RequestPolicy,
}

impl LastFmClient {
    pub fn new(credentials: AppCredentials) -> Result<Self, LastFmClientError> {
        let endpoint =
            Url::parse(API_ENDPOINT).map_err(|_| LastFmClientError::ClientConstruction)?;
        if endpoint.as_str() != API_ENDPOINT
            || endpoint.scheme() != "https"
            || endpoint.host_str() != Some("ws.audioscrobbler.com")
            || endpoint.port_or_known_default() != Some(443)
        {
            return Err(LastFmClientError::ClientConstruction);
        }
        Self::with_endpoint_and_policy(endpoint, credentials, RequestPolicy::PRODUCTION)
    }

    fn with_endpoint_and_policy(
        endpoint: Url,
        credentials: AppCredentials,
        policy: RequestPolicy,
    ) -> Result<Self, LastFmClientError> {
        let http = crate::http_security::authenticated_client_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(policy.timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| LastFmClientError::ClientConstruction)?;
        Ok(Self {
            endpoint,
            http,
            credentials,
            policy,
        })
    }

    #[cfg(test)]
    fn for_test(endpoint: &str, credentials: AppCredentials) -> Self {
        Self::with_test_policy(endpoint, credentials, RequestPolicy::PRODUCTION)
    }

    #[cfg(test)]
    fn with_test_policy(
        endpoint: &str,
        credentials: AppCredentials,
        policy: RequestPolicy,
    ) -> Self {
        let endpoint = Url::parse(endpoint).expect("fixture endpoint URL");
        assert!(
            endpoint.host_str() == Some("127.0.0.1") && endpoint.scheme() == "http",
            "test endpoint must be plaintext IPv4 loopback"
        );
        Self::with_endpoint_and_policy(endpoint, credentials, policy).expect("fixture client")
    }

    /// Begin the documented desktop authorization flow.
    pub(super) async fn request_auth_token(&self) -> Result<DesktopAuthToken, LastFmClientError> {
        let response: TokenResponse = self
            .signed_auth_post(vec![("method".to_string(), "auth.getToken".to_string())])
            .await?;
        DesktopAuthToken::from_response(response.token)
    }

    /// Build the exact HTTPS Last.fm browser-authorization URL.
    pub(super) fn authorization_url(
        &self,
        token: &DesktopAuthToken,
    ) -> Result<DesktopAuthorizationUrl, LastFmClientError> {
        let mut url = Url::parse(AUTHORIZATION_ENDPOINT)
            .map_err(|_| LastFmClientError::ClientConstruction)?;
        if url.as_str() != AUTHORIZATION_ENDPOINT
            || url.scheme() != "https"
            || url.host_str() != Some("www.last.fm")
            || url.port_or_known_default() != Some(443)
        {
            return Err(LastFmClientError::ClientConstruction);
        }
        url.query_pairs_mut()
            .append_pair("api_key", self.credentials.api_key.expose())
            .append_pair("token", token.0.expose());
        // `From<Url> for String` moves Url's serialization allocation instead
        // of formatting a second token-bearing copy. ProtectedString wipes the
        // resulting URL, including its API key and request token, on drop.
        Ok(DesktopAuthorizationUrl(ProtectedString::new(String::from(
            url,
        ))))
    }

    /// Exchange a user-authorized desktop token for a staged session.
    ///
    /// Taking the non-cloneable token by value is the low-level one-shot
    /// boundary. The returned identity remains unbound until the serialized
    /// authorization owner applies exact account/replacement policy.
    pub(super) async fn exchange_auth_token(
        &self,
        token: DesktopAuthToken,
    ) -> Result<DesktopAuthorizedSession, LastFmClientError> {
        let response: SessionEnvelope = self
            .signed_auth_post(vec![
                ("method".to_string(), "auth.getSession".to_string()),
                ("token".to_string(), token.0.expose().to_string()),
            ])
            .await?;
        DesktopAuthorizedSession::from_response(response.session)
    }

    /// Publish metadata for the track currently playing.
    pub async fn update_now_playing(
        &self,
        session: &StoredSession,
        track: &LastFmTrack,
    ) -> Result<SubmissionResult, LastFmClientError> {
        validate_session(session)?;
        validate_track(track)?;
        let mut parameters = authenticated_parameters("track.updateNowPlaying", session);
        append_track_parameters(&mut parameters, track, None);
        let response: NowPlayingEnvelope = self.signed_post(parameters).await?;
        submission_result(&response.nowplaying)
    }

    /// Submit up to 50 completed plays in caller-provided order.
    pub async fn scrobble(
        &self,
        session: &StoredSession,
        scrobbles: &[Scrobble],
    ) -> Result<ScrobbleBatchResult, LastFmClientError> {
        validate_session(session)?;
        if scrobbles.is_empty() || scrobbles.len() > MAX_SCROBBLES_PER_BATCH {
            return Err(LastFmClientError::InvalidInput);
        }
        let mut parameters = authenticated_parameters("track.scrobble", session);
        for (index, scrobble) in scrobbles.iter().enumerate() {
            validate_track(&scrobble.track)?;
            if scrobble.started_at_unix_seconds == 0 {
                return Err(LastFmClientError::InvalidInput);
            }
            append_track_parameters(&mut parameters, &scrobble.track, Some(index));
            parameters.push((
                indexed("timestamp", index),
                scrobble.started_at_unix_seconds.to_string(),
            ));
        }

        let response: ScrobblesEnvelope = self.signed_post(parameters).await?;
        let raw_items = response.scrobbles.scrobble.into_vec();
        if raw_items.len() != scrobbles.len() {
            return Err(LastFmClientError::InvalidResponse);
        }
        let items: Vec<_> = raw_items
            .iter()
            .map(submission_result)
            .collect::<Result<_, _>>()?;
        let accepted = parse_count(&response.scrobbles.attributes.accepted)?;
        let ignored = parse_count(&response.scrobbles.attributes.ignored)?;
        let observed_accepted = items
            .iter()
            .filter(|item| matches!(item, SubmissionResult::Accepted { .. }))
            .count();
        if accepted != observed_accepted
            || ignored != items.len().saturating_sub(observed_accepted)
            || accepted.saturating_add(ignored) != items.len()
        {
            return Err(LastFmClientError::InvalidResponse);
        }
        Ok(ScrobbleBatchResult { items })
    }

    async fn signed_post<T: DeserializeOwned>(
        &self,
        parameters: Vec<(String, String)>,
    ) -> Result<T, LastFmClientError> {
        let (status, body) = self.signed_response(parameters).await?;

        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
            if status.is_success() {
                LastFmClientError::InvalidResponse
            } else {
                status_error(status)
            }
        })?;
        if let Some(error) = provider_error(&value)? {
            return Err(error);
        }
        if !status.is_success() {
            return Err(status_error(status));
        }
        serde_json::from_value(value).map_err(|_| LastFmClientError::InvalidResponse)
    }

    /// Parse an authentication response without first materializing a generic
    /// JSON value whose ordinary strings could retain a token, username, key,
    /// or provider message. The first pass borrows only the provider error
    /// code; the typed success pass owns sensitive fields directly inside
    /// zeroizing wrappers.
    async fn signed_auth_post<T: AuthResponse>(
        &self,
        parameters: Vec<(String, String)>,
    ) -> Result<T, LastFmClientError> {
        let (status, body) = self.signed_response(parameters).await?;
        parse_auth_response(status, &body)
    }

    async fn signed_response(
        &self,
        parameters: Vec<(String, String)>,
    ) -> Result<(StatusCode, Zeroizing<Vec<u8>>), LastFmClientError> {
        let mut parameters = SensitiveParameters(parameters);
        parameters.0.push((
            "api_key".to_string(),
            self.credentials.api_key.expose().to_string(),
        ));
        let signature = sign_parameters(&parameters.0, self.credentials.shared_secret.expose())?;
        parameters.0.push(("api_sig".to_string(), signature));
        parameters
            .0
            .push(("format".to_string(), "json".to_string()));

        let encoded = encode_form(&parameters.0, self.policy.maximum_form_bytes)?;

        let response = self
            .http
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .timeout(self.policy.timeout)
            .body(encoded.as_bytes().to_vec())
            .send()
            .await;
        let response = response.map_err(map_reqwest_error)?;
        let status = response.status();
        let body = Zeroizing::new(
            read_limited(
                response,
                self.policy.maximum_response_bytes,
                self.policy.timeout,
            )
            .await
            .map_err(map_body_error)?,
        );
        Ok((status, body))
    }
}

trait AuthResponse: Sized {
    fn from_auth_raw(raw: &RawValue) -> Result<Self, LastFmClientError>;
}

fn auth_object(raw: &RawValue) -> Result<&str, LastFmClientError> {
    let representation = raw.get();
    if representation.as_bytes().first() == Some(&b'{') {
        Ok(representation)
    } else {
        Err(LastFmClientError::InvalidResponse)
    }
}

fn parse_auth_response<T: AuthResponse>(
    status: StatusCode,
    body: &[u8],
) -> Result<T, LastFmClientError> {
    // RawValue validates one complete JSON value while borrowing the original
    // zeroizing response body. Its scanner intentionally does not validate
    // Unicode surrogate pairing, so perform that allocation-free pass before
    // interpreting either provider errors or success fields.
    let Some(raw) = validated_auth_raw(body) else {
        return Err(if status.is_success() {
            LastFmClientError::InvalidResponse
        } else {
            status_error(status)
        });
    };

    if raw.get().starts_with('{') {
        let probe: AuthProviderErrorProbe =
            serde_json::from_str(raw.get()).map_err(|_| LastFmClientError::InvalidResponse)?;
        if let Some(code) = probe.error {
            return Err(provider_error_for_code(parse_auth_provider_code(code)?)?);
        }
    }
    if !status.is_success() {
        return Err(status_error(status));
    }
    T::from_auth_raw(raw)
}

fn validated_auth_raw(body: &[u8]) -> Option<&RawValue> {
    let raw: &RawValue = serde_json::from_slice(body).ok()?;
    validate_json_string_literals(raw.get()).ok()?;
    Some(raw)
}

#[derive(Clone, Copy, Debug)]
struct InvalidAuthJsonString;

fn validate_json_string_literals(raw: &str) -> Result<(), InvalidAuthJsonString> {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let end = json_string_literal_end(bytes, index).ok_or(InvalidAuthJsonString)?;
            visit_json_string_literal(&raw[index..end], |_| {})?;
            index = end;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn json_string_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut index = start.checked_add(1)?;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'"' => return index.checked_add(1),
            b'\\' => {
                let escape = *bytes.get(index.checked_add(1)?)?;
                index = index.checked_add(if escape == b'u' { 6 } else { 2 })?;
                if index > bytes.len() {
                    return None;
                }
            }
            _ => index += 1,
        }
    }
    None
}

fn visit_json_string_literal(
    literal: &str,
    mut visit: impl FnMut(char),
) -> Result<(), InvalidAuthJsonString> {
    let inner = literal
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or(InvalidAuthJsonString)?;
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            if character == '"' || character <= '\u{001f}' {
                return Err(InvalidAuthJsonString);
            }
            visit(character);
            continue;
        }

        let escaped = match characters.next().ok_or(InvalidAuthJsonString)? {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{0008}',
            'f' => '\u{000c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => decode_json_unicode_escape(&mut characters)?,
            _ => return Err(InvalidAuthJsonString),
        };
        visit(escaped);
    }
    Ok(())
}

fn decode_json_unicode_escape(
    characters: &mut std::str::Chars<'_>,
) -> Result<char, InvalidAuthJsonString> {
    let leading = decode_json_hex_quad(characters)?;
    let scalar = match leading {
        0xd800..=0xdbff => {
            if characters.next() != Some('\\') || characters.next() != Some('u') {
                return Err(InvalidAuthJsonString);
            }
            let trailing = decode_json_hex_quad(characters)?;
            if !(0xdc00..=0xdfff).contains(&trailing) {
                return Err(InvalidAuthJsonString);
            }
            0x1_0000 + ((u32::from(leading) - 0xd800) << 10) + (u32::from(trailing) - 0xdc00)
        }
        0xdc00..=0xdfff => return Err(InvalidAuthJsonString),
        _ => u32::from(leading),
    };
    char::from_u32(scalar).ok_or(InvalidAuthJsonString)
}

fn decode_json_hex_quad(
    characters: &mut std::str::Chars<'_>,
) -> Result<u16, InvalidAuthJsonString> {
    let mut value = 0_u16;
    for _ in 0..4 {
        let character = characters.next().ok_or(InvalidAuthJsonString)?;
        if !character.is_ascii_hexdigit() {
            return Err(InvalidAuthJsonString);
        }
        value = (value << 4)
            | u16::try_from(character.to_digit(16).ok_or(InvalidAuthJsonString)?)
                .map_err(|_| InvalidAuthJsonString)?;
    }
    Ok(value)
}

fn encode_form(
    parameters: &[(String, String)],
    maximum_bytes: usize,
) -> Result<Zeroizing<String>, LastFmClientError> {
    encode_form_with_observer(parameters, maximum_bytes, |_| {})
}

fn encode_form_with_observer(
    parameters: &[(String, String)],
    maximum_bytes: usize,
    mut observe: impl FnMut(&String),
) -> Result<Zeroizing<String>, LastFmClientError> {
    let encoded_length = encoded_form_length(parameters, maximum_bytes)?;
    let mut encoded = Zeroizing::new(String::new());
    encoded
        .try_reserve_exact(encoded_length)
        .map_err(|_| LastFmClientError::InvalidInput)?;
    observe(&encoded);

    {
        let mut serializer = url::form_urlencoded::Serializer::new(&mut *encoded);
        serializer.extend_pairs(parameters.iter().map(|(key, value)| (&**key, &**value)));
    }
    observe(&encoded);
    debug_assert_eq!(encoded.len(), encoded_length);
    Ok(encoded)
}

fn encoded_form_length(
    parameters: &[(String, String)],
    maximum_bytes: usize,
) -> Result<usize, LastFmClientError> {
    let maximum_bytes = maximum_bytes.min(MAX_FORM_BODY_BYTES);
    let mut length = 0;
    for (index, (key, value)) in parameters.iter().enumerate() {
        if index != 0 {
            length = checked_form_length(length, 1, maximum_bytes)?;
        }
        length = checked_encoded_component_length(length, key, maximum_bytes)?;
        length = checked_form_length(length, 1, maximum_bytes)?;
        length = checked_encoded_component_length(length, value, maximum_bytes)?;
    }
    Ok(length)
}

fn checked_encoded_component_length(
    mut length: usize,
    component: &str,
    maximum_bytes: usize,
) -> Result<usize, LastFmClientError> {
    for encoded in url::form_urlencoded::byte_serialize(component.as_bytes()) {
        length = checked_form_length(length, encoded.len(), maximum_bytes)?;
    }
    Ok(length)
}

fn checked_form_length(
    length: usize,
    additional: usize,
    maximum_bytes: usize,
) -> Result<usize, LastFmClientError> {
    let length = length
        .checked_add(additional)
        .ok_or(LastFmClientError::InvalidInput)?;
    if length > maximum_bytes {
        return Err(LastFmClientError::InvalidInput);
    }
    Ok(length)
}

fn authenticated_parameters(method: &str, session: &StoredSession) -> Vec<(String, String)> {
    vec![
        ("method".to_string(), method.to_string()),
        ("sk".to_string(), session.key().expose().to_string()),
    ]
}

fn append_track_parameters(
    parameters: &mut Vec<(String, String)>,
    track: &LastFmTrack,
    index: Option<usize>,
) {
    let mut push = |name: &str, value: String| {
        parameters.push((
            index.map_or_else(|| name.to_string(), |index| indexed(name, index)),
            value,
        ));
    };
    push("artist", track.artist.clone());
    push("track", track.title.clone());
    if let Some(album) = track
        .album
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push("album", album.to_string());
    }
    if let Some(album_artist) = track
        .album_artist
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        push("albumArtist", album_artist.to_string());
    }
    if let Some(track_number) = track.track_number {
        push("trackNumber", track_number.to_string());
    }
    push("duration", track.duration_seconds.to_string());
}

fn indexed(name: &str, index: usize) -> String {
    format!("{name}[{index}]")
}

fn sign_parameters(
    parameters: &[(String, String)],
    shared_secret: &str,
) -> Result<String, LastFmClientError> {
    validate_hex_credential(shared_secret)?;
    let mut signed: Vec<_> = parameters
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "api_sig" | "callback" | "format"))
        .collect();
    signed.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let estimated = signed
        .iter()
        .try_fold(shared_secret.len(), |total, (name, value)| {
            total.checked_add(name.len())?.checked_add(value.len())
        });
    let Some(estimated) = estimated.filter(|estimated| *estimated <= MAX_FORM_BODY_BYTES) else {
        return Err(LastFmClientError::InvalidInput);
    };
    let mut source = String::new();
    source
        .try_reserve(estimated)
        .map_err(|_| LastFmClientError::InvalidInput)?;
    for (name, value) in signed {
        source.push_str(name);
        source.push_str(value);
    }
    source.push_str(shared_secret);
    let digest = Md5::digest(source.as_bytes());
    source.zeroize();
    Ok(digest
        .iter()
        .fold(String::with_capacity(32), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        }))
}

fn validate_session(session: &StoredSession) -> Result<(), LastFmClientError> {
    validate_text(session.username(), MAX_USERNAME_BYTES, true)?;
    validate_hex_credential(session.key().expose())
}

fn validate_track(track: &LastFmTrack) -> Result<(), LastFmClientError> {
    validate_required_text(&track.artist, MAX_METADATA_BYTES)?;
    validate_required_text(&track.title, MAX_METADATA_BYTES)?;
    for value in [track.album.as_deref(), track.album_artist.as_deref()]
        .into_iter()
        .flatten()
    {
        if !value.trim().is_empty() {
            validate_text(value, MAX_METADATA_BYTES, true)?;
        }
    }
    if matches!(track.track_number, Some(0)) || track.duration_seconds <= 30 {
        return Err(LastFmClientError::InvalidInput);
    }
    Ok(())
}

fn validate_required_text(value: &str, maximum_bytes: usize) -> Result<(), LastFmClientError> {
    validate_text(value, maximum_bytes, true)?;
    if value.trim().is_empty() {
        Err(LastFmClientError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_text(
    value: &str,
    maximum_bytes: usize,
    reject_controls: bool,
) -> Result<(), LastFmClientError> {
    if value.is_empty()
        || value.len() > maximum_bytes
        || value.contains('\0')
        || (reject_controls && value.chars().any(char::is_control))
    {
        Err(LastFmClientError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_hex_credential(value: &str) -> Result<(), LastFmClientError> {
    if value.len() == EXPECTED_CREDENTIAL_BYTES
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(LastFmClientError::InvalidInput)
    }
}

fn map_reqwest_error(error: reqwest::Error) -> LastFmClientError {
    if error.is_timeout() {
        LastFmClientError::Timeout
    } else {
        LastFmClientError::Transport
    }
}

fn map_body_error(error: ResponseBodyError) -> LastFmClientError {
    match error {
        ResponseBodyError::DeadlineExceeded { .. } => LastFmClientError::Timeout,
        ResponseBodyError::Transport(error) if error.is_timeout() => LastFmClientError::Timeout,
        ResponseBodyError::Transport(_) | ResponseBodyError::BlockingTransport { .. } => {
            LastFmClientError::Transport
        }
        ResponseBodyError::TooLarge { .. }
        | ResponseBodyError::InvalidLimit { .. }
        | ResponseBodyError::AllocationFailed { .. } => LastFmClientError::BodyLimit,
    }
}

fn status_error(status: StatusCode) -> LastFmClientError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        LastFmClientError::RateLimited
    } else if status.is_server_error() {
        LastFmClientError::ServiceUnavailable
    } else {
        LastFmClientError::HttpStatus
    }
}

fn provider_error(
    value: &serde_json::Value,
) -> Result<Option<LastFmClientError>, LastFmClientError> {
    let Some(code) = value.get("error") else {
        return Ok(None);
    };
    let code = code
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .or_else(|| code.as_str().and_then(|value| value.parse::<u16>().ok()))
        .ok_or(LastFmClientError::InvalidResponse)?;
    Ok(Some(provider_error_for_code(code)?))
}

fn provider_error_for_code(code: u16) -> Result<LastFmClientError, LastFmClientError> {
    Ok(match code {
        9 => LastFmClientError::ReauthenticationRequired,
        // Last.fm's published registry explicitly asks clients to retry the
        // generic backend failure (8) and temporary outages (11/16).
        8 | 11 | 16 => LastFmClientError::ServiceUnavailable,
        29 => LastFmClientError::RateLimited,
        // Exhaustive recognized set from Last.fm's published error-code
        // registry. Unknown future values are compatibility failures rather
        // than guessed terminal results.
        1..=7 | 10 | 12..=15 | 17..=27 => LastFmClientError::ServiceRejected { code },
        _ => return Err(LastFmClientError::InvalidResponse),
    })
}

fn submission_result(item: &RawSubmission) -> Result<SubmissionResult, LastFmClientError> {
    let code = parse_u16(&item.ignored_message.code)?;
    if code == 0 {
        let artist = item
            .artist
            .as_ref()
            .ok_or(LastFmClientError::InvalidResponse)?;
        let track = item
            .track
            .as_ref()
            .ok_or(LastFmClientError::InvalidResponse)?;
        Ok(SubmissionResult::Accepted {
            corrected: [
                Some(artist),
                Some(track),
                item.album.as_ref(),
                item.album_artist.as_ref(),
            ]
            .into_iter()
            .flatten()
            .try_fold(false, |corrected, value| {
                parse_corrected(value).map(|value| corrected || value)
            })?,
        })
    } else {
        Ok(SubmissionResult::Ignored {
            reason: match code {
                1 => IgnoredReason::Artist,
                2 => IgnoredReason::Track,
                3 => IgnoredReason::TimestampTooOld,
                4 => IgnoredReason::TimestampTooNew,
                5 => IgnoredReason::DailyLimit,
                code => IgnoredReason::Other(code),
            },
        })
    }
}

fn parse_corrected(value: &RawText) -> Result<bool, LastFmClientError> {
    match value.corrected.as_ref() {
        None => Ok(false),
        Some(StringOrNumber::String(value)) => match value.as_str() {
            "0" => Ok(false),
            "1" => Ok(true),
            _ => Err(LastFmClientError::InvalidResponse),
        },
        Some(StringOrNumber::Number(0)) => Ok(false),
        Some(StringOrNumber::Number(1)) => Ok(true),
        Some(StringOrNumber::Number(_)) => Err(LastFmClientError::InvalidResponse),
    }
}

fn parse_count(value: &StringOrNumber) -> Result<usize, LastFmClientError> {
    Ok(usize::from(parse_u16(value)?))
}

fn parse_u16(value: &StringOrNumber) -> Result<u16, LastFmClientError> {
    match value {
        StringOrNumber::String(value) => value
            .parse::<u16>()
            .map_err(|_| LastFmClientError::InvalidResponse),
        StringOrNumber::Number(value) => Ok(*value),
    }
}

/// Borrowing provider-error probe for secret-bearing auth responses.
///
/// The custom map visitor rejects a duplicate `error` member and borrows every
/// value as raw JSON. Consequently neither success secrets nor ignored provider
/// text pass through serde_json's ordinary string scratch during this pass.
struct AuthProviderErrorProbe<'a> {
    error: Option<&'a RawValue>,
}

impl<'de> Deserialize<'de> for AuthProviderErrorProbe<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(AuthProviderErrorProbeVisitor)
    }
}

struct AuthProviderErrorProbeVisitor;

impl<'de> Visitor<'de> for AuthProviderErrorProbeVisitor {
    type Value = AuthProviderErrorProbe<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Last.fm authentication response object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut error = None;
        while let Some(field) = map.next_key::<AuthProviderField>()? {
            let value: &'de RawValue = map.next_value()?;
            if field == AuthProviderField::Error && error.replace(value).is_some() {
                return Err(serde::de::Error::duplicate_field("error"));
            }
        }
        Ok(AuthProviderErrorProbe { error })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AuthProviderField {
    Error,
    Other,
}

impl<'de> Deserialize<'de> for AuthProviderField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_identifier(AuthProviderFieldVisitor)
    }
}

struct AuthProviderFieldVisitor;

impl Visitor<'_> for AuthProviderFieldVisitor {
    type Value = AuthProviderField;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Last.fm authentication response field")
    }

    fn visit_borrowed_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(auth_provider_field(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(auth_provider_field(value))
    }
}

fn auth_provider_field(value: &str) -> AuthProviderField {
    if value == "error" {
        AuthProviderField::Error
    } else {
        AuthProviderField::Other
    }
}

fn parse_auth_provider_code(raw: &RawValue) -> Result<u16, LastFmClientError> {
    let representation = raw.get();
    if !representation.starts_with('"') {
        return representation
            .parse::<u16>()
            .map_err(|_| LastFmClientError::InvalidResponse);
    }

    let mut parsed = Some(0_u16);
    let mut digits = 0_usize;
    let mut position = 0_usize;
    visit_json_string_literal(representation, |character| {
        if position == 0 && character == '+' {
            // Match `u16::from_str`, which permits one leading plus sign.
        } else if character.is_ascii_digit() {
            digits += 1;
            let digit = u16::from(character as u8 - b'0');
            parsed = parsed
                .and_then(|value| value.checked_mul(10))
                .and_then(|value| value.checked_add(digit));
        } else {
            parsed = None;
        }
        position += 1;
    })
    .map_err(|_| LastFmClientError::InvalidResponse)?;
    parsed
        .filter(|_| digits > 0)
        .ok_or(LastFmClientError::InvalidResponse)
}

/// A JSON response string protected from its first owned allocation.
struct SecretResponseString(Zeroizing<String>);

impl SecretResponseString {
    fn from_json_literal(raw: &RawValue) -> Result<Self, LastFmClientError> {
        let representation = raw.get();
        let inner = representation
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(LastFmClientError::InvalidResponse)?;
        let mut value = Zeroizing::new(String::with_capacity(inner.len()));
        visit_json_string_literal(representation, |character| value.push(character))
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        Ok(Self(value))
    }

    fn into_zeroizing(self) -> Zeroizing<String> {
        self.0
    }

    fn take(&mut self) -> String {
        std::mem::take(&mut *self.0)
    }

    #[cfg(test)]
    fn for_test(value: &str) -> Self {
        Self(Zeroizing::new(value.to_owned()))
    }
}

impl fmt::Debug for SecretResponseString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretResponseString([REDACTED])")
    }
}

struct TokenResponse {
    token: SecretResponseString,
}

struct SessionEnvelope {
    session: RawSession,
}

struct RawSession {
    name: SecretResponseString,
    key: SecretResponseString,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedTokenResponse<'a> {
    #[serde(borrow)]
    token: &'a RawValue,
}

impl AuthResponse for TokenResponse {
    fn from_auth_raw(raw: &RawValue) -> Result<Self, LastFmClientError> {
        // Serde-derived structs also accept positional sequences. Inspect the
        // borrowed raw shape before deserialization so a scalar or sequence
        // cannot enter serde_json's ordinary string scratch.
        let response: BorrowedTokenResponse = serde_json::from_str(auth_object(raw)?)
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        Ok(Self {
            token: SecretResponseString::from_json_literal(response.token)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedSessionEnvelope<'a> {
    #[serde(borrow)]
    session: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BorrowedRawSession<'a> {
    #[serde(borrow)]
    name: &'a RawValue,
    #[serde(borrow)]
    key: &'a RawValue,
    #[serde(borrow, default, rename = "subscriber")]
    _subscriber: Option<&'a RawValue>,
}

impl AuthResponse for SessionEnvelope {
    fn from_auth_raw(raw: &RawValue) -> Result<Self, LastFmClientError> {
        let envelope: BorrowedSessionEnvelope = serde_json::from_str(auth_object(raw)?)
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        let session: BorrowedRawSession = serde_json::from_str(auth_object(envelope.session)?)
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        Ok(Self {
            session: RawSession {
                name: SecretResponseString::from_json_literal(session.name)?,
                key: SecretResponseString::from_json_literal(session.key)?,
            },
        })
    }
}

#[derive(Deserialize)]
struct NowPlayingEnvelope {
    nowplaying: RawSubmission,
}

#[derive(Deserialize)]
struct ScrobblesEnvelope {
    scrobbles: RawScrobbles,
}

#[derive(Deserialize)]
struct RawScrobbles {
    #[serde(rename = "@attr")]
    attributes: RawScrobbleCounts,
    scrobble: OneOrMany<RawSubmission>,
}

#[derive(Deserialize)]
struct RawScrobbleCounts {
    accepted: StringOrNumber,
    ignored: StringOrNumber,
}

#[derive(Deserialize)]
struct RawSubmission {
    #[serde(default)]
    artist: Option<RawText>,
    #[serde(default)]
    track: Option<RawText>,
    #[serde(default)]
    album: Option<RawText>,
    #[serde(rename = "albumArtist", default)]
    album_artist: Option<RawText>,
    #[serde(rename = "ignoredMessage")]
    ignored_message: RawIgnoredMessage,
}

#[derive(Deserialize)]
struct RawText {
    #[serde(default)]
    corrected: Option<StringOrNumber>,
}

#[derive(Deserialize)]
struct RawIgnoredMessage {
    code: StringOrNumber,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    String(String),
    Number(u16),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use axum::http::header::{CONTENT_TYPE, LOCATION, REFERER};
    use axum::http::{HeaderValue, Method, StatusCode};
    use serde_json::json;
    use zeroize::Zeroizing;

    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::{
        append_track_parameters, encode_form_with_observer, provider_error, sign_parameters,
        AppCredentials, IgnoredReason, LastFmClient, LastFmClientError, LastFmTrack, RequestPolicy,
        Scrobble, ScrobblesEnvelope, SubmissionResult, MAX_FORM_BODY_BYTES,
        MAX_RESPONSE_BODY_BYTES, MAX_SCROBBLES_PER_BATCH,
    };

    const API_KEY: &str = "0123456789abcdef0123456789abcdef";
    const SHARED_SECRET: &str = "abcdef0123456789abcdef0123456789";
    const TOKEN: &str = "fedcba9876543210fedcba9876543210";
    const SESSION_KEY: &str = "11111111111111111111111111111111";

    fn credentials() -> AppCredentials {
        AppCredentials::for_test(API_KEY, SHARED_SECRET).expect("fixture credentials")
    }

    fn track(index: usize) -> LastFmTrack {
        LastFmTrack {
            artist: format!("Artist {index}"),
            title: format!("Track {index}"),
            album: Some("Album".to_string()),
            album_artist: Some("Album Artist".to_string()),
            track_number: Some(u32::try_from(index + 1).expect("small fixture index")),
            duration_seconds: 240,
        }
    }

    fn accepted_submission(corrected: &str) -> serde_json::Value {
        json!({
            "track": {"corrected": corrected, "#text": "not retained"},
            "artist": {"corrected": "0", "#text": "not retained"},
            "album": {"corrected": "0", "#text": "not retained"},
            "albumArtist": {"corrected": "0", "#text": "not retained"},
            "ignoredMessage": {"code": "0", "#text": ""}
        })
    }

    fn accepted_submission_numeric(corrected: u16) -> serde_json::Value {
        json!({
            "track": {"corrected": corrected, "#text": "not retained"},
            "artist": {"corrected": 0, "#text": "not retained"},
            "album": {"corrected": 0, "#text": "not retained"},
            "albumArtist": {"corrected": 0, "#text": "not retained"},
            "ignoredMessage": {"code": 0, "#text": ""}
        })
    }

    fn ignored_submission(code: u16) -> serde_json::Value {
        json!({
            "track": {"corrected": "0", "#text": "not retained"},
            "artist": {"corrected": "0", "#text": "not retained"},
            "ignoredMessage": {"code": code.to_string(), "#text": "provider secret"}
        })
    }

    fn form(request_body: &[u8]) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(request_body)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[test]
    fn signing_is_ascii_sorted_and_excludes_response_format() {
        let parameters = vec![
            ("token".to_string(), TOKEN.to_string()),
            ("format".to_string(), "json".to_string()),
            ("method".to_string(), "auth.getSession".to_string()),
            ("api_key".to_string(), API_KEY.to_string()),
        ];
        assert_eq!(
            sign_parameters(&parameters, "00000000000000000000000000000000")
                .expect("parameters sign"),
            "829fe7fee1f9841377cc16c83389968a"
        );
    }

    #[test]
    fn corrected_flags_accept_only_string_or_numeric_boolean_values() {
        for (fixture, expected) in [
            (json!({}), false),
            (json!({"corrected": "0"}), false),
            (json!({"corrected": "1"}), true),
            (json!({"corrected": 0}), false),
            (json!({"corrected": 1}), true),
        ] {
            let parsed: super::RawText = serde_json::from_value(fixture).unwrap();
            assert_eq!(super::parse_corrected(&parsed).unwrap(), expected);
        }
        for invalid in [json!({"corrected": "2"}), json!({"corrected": 2})] {
            let parsed: super::RawText = serde_json::from_value(invalid).unwrap();
            assert_eq!(
                super::parse_corrected(&parsed).unwrap_err(),
                LastFmClientError::InvalidResponse
            );
        }
    }

    #[test]
    fn maximum_fifty_row_form_fits_the_production_cap_after_percent_encoding() {
        // Four-byte Unicode guarantees every one of the 1,024 admitted bytes
        // expands to `%XX`, exercising the form encoder's maximum 3x ratio.
        let maximum_value = "💿".repeat(super::MAX_METADATA_BYTES / "💿".len());
        assert_eq!(maximum_value.len(), super::MAX_METADATA_BYTES);
        let maximum_track = LastFmTrack {
            artist: maximum_value.clone(),
            title: maximum_value.clone(),
            album: Some(maximum_value.clone()),
            album_artist: Some(maximum_value),
            track_number: Some(u32::MAX),
            duration_seconds: u32::MAX,
        };
        let mut parameters = vec![
            ("method".to_string(), "track.scrobble".to_string()),
            ("sk".to_string(), SESSION_KEY.to_string()),
            ("api_key".to_string(), API_KEY.to_string()),
        ];
        for index in 0..MAX_SCROBBLES_PER_BATCH {
            append_track_parameters(&mut parameters, &maximum_track, Some(index));
            parameters.push((super::indexed("timestamp", index), u64::MAX.to_string()));
        }
        let signature = sign_parameters(&parameters, SHARED_SECRET).expect("maximum form signs");
        parameters.push(("api_sig".to_string(), signature));
        parameters.push(("format".to_string(), "json".to_string()));

        let mut allocation_states = Vec::with_capacity(2);
        let encoded =
            encode_form_with_observer(&parameters, MAX_FORM_BODY_BYTES, |partially_encoded| {
                allocation_states.push((partially_encoded.as_ptr(), partially_encoded.capacity()));
            })
            .expect("maximum form encodes");
        assert!(
            encoded.len() > 512 * 1024,
            "fixture must regress the old undersized cap"
        );
        assert!(encoded.len() <= MAX_FORM_BODY_BYTES);
        assert_eq!(allocation_states.len(), 2);
        assert!(allocation_states[0].1 >= encoded.len());
        assert!(
            allocation_states
                .iter()
                .all(|state| *state == allocation_states[0]),
            "the protected form allocation must remain stable during secret-bearing serialization"
        );
        assert_eq!(
            form(encoded.as_bytes()).get("artist[49]").map(String::len),
            Some(super::MAX_METADATA_BYTES)
        );
    }

    #[test]
    fn auth_form_reserves_before_appending_the_complete_token() {
        let parameters = vec![
            ("method".to_string(), "auth.getSession".to_string()),
            ("token".to_string(), TOKEN.to_string()),
            ("api_key".to_string(), API_KEY.to_string()),
            ("api_sig".to_string(), SHARED_SECRET.to_string()),
            ("format".to_string(), "json".to_string()),
        ];
        let mut allocation_states = Vec::with_capacity(2);
        let encoded =
            encode_form_with_observer(&parameters, MAX_FORM_BODY_BYTES, |partially_encoded| {
                allocation_states.push((partially_encoded.as_ptr(), partially_encoded.capacity()));
            })
            .expect("auth form encodes");

        assert_eq!(allocation_states.len(), 2);
        assert!(allocation_states[0].1 >= encoded.len());
        assert!(
            allocation_states
                .iter()
                .all(|state| *state == allocation_states[0]),
            "the protected form allocation must remain stable after the token is serialized"
        );
        assert_eq!(
            form(encoded.as_bytes()).get("token").map(String::as_str),
            Some(TOKEN)
        );
    }

    #[test]
    fn form_cap_failure_precedes_protected_serialization() {
        let parameters = vec![("token".to_string(), TOKEN.to_string())];
        let mut observations = 0;
        let result = encode_form_with_observer(&parameters, 1, |_| observations += 1);

        assert!(matches!(result, Err(LastFmClientError::InvalidInput)));
        assert_eq!(observations, 0);
    }

    #[tokio::test]
    async fn response_cap_covers_fifty_maximal_json_escaped_echoes() {
        // JSON permits any printable ASCII input byte to be serialized as a
        // six-byte Unicode escape. Last.fm echoes four submitted metadata
        // fields, so this is the protocol-valid expansion bound, not merely
        // serde_json's usual two-byte quote/backslash escaping behavior.
        let escaped_value = "\\u0021".repeat(super::MAX_METADATA_BYTES);
        let field = format!(r##"{{"corrected":"0","#text":"{escaped_value}"}}"##);
        let item = format!(
            "{{\"track\":{field},\"artist\":{field},\"album\":{field},\"albumArtist\":{field},\"ignoredMessage\":{{\"code\":\"0\",\"#text\":\"\"}}}}"
        );
        let items = std::iter::repeat_n(item, MAX_SCROBBLES_PER_BATCH)
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(
            "{{\"scrobbles\":{{\"@attr\":{{\"accepted\":\"50\",\"ignored\":\"0\"}},\"scrobble\":[{items}]}}}}"
        )
        .into_bytes();
        assert!(
            body.len() > 1024 * 1024,
            "fixture must demonstrate why a one-MiB response cap is insufficient"
        );
        assert!(body.len() <= usize::try_from(MAX_RESPONSE_BODY_BYTES).unwrap());

        let response: reqwest::Response = http::Response::builder()
            .status(StatusCode::OK)
            .body(body)
            .expect("bounded response fixture")
            .into();
        let collected = Zeroizing::new(
            super::read_limited(response, MAX_RESPONSE_BODY_BYTES, Duration::from_secs(1))
                .await
                .expect("maximum valid response fits cap"),
        );
        let parsed: ScrobblesEnvelope =
            serde_json::from_slice(&collected).expect("maximum response parses");
        assert_eq!(
            parsed.scrobbles.scrobble.into_vec().len(),
            MAX_SCROBBLES_PER_BATCH
        );
    }

    #[test]
    fn app_credentials_and_errors_are_redacted_and_strict() {
        let credentials = credentials();
        assert_eq!(format!("{credentials:?}"), "AppCredentials([REDACTED])");
        for invalid in ["", "short", "gggggggggggggggggggggggggggggggg"] {
            assert_eq!(
                AppCredentials::for_test(invalid, SHARED_SECRET).err(),
                Some(LastFmClientError::InvalidInput)
            );
        }
        for error in [
            LastFmClientError::AppCredentialsUnavailable,
            LastFmClientError::Transport,
            LastFmClientError::ServiceRejected { code: 13 },
        ] {
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains(API_KEY));
            assert!(!rendered.contains(SHARED_SECRET));
        }
    }

    #[test]
    fn authorization_test_fixtures_are_strict_and_redacted() {
        let token = super::DesktopAuthToken::for_test(TOKEN).expect("valid fixture token");
        assert_eq!(format!("{token:?}"), "DesktopAuthToken([REDACTED])");
        assert_eq!(
            super::DesktopAuthToken::for_test("not-a-token").err(),
            Some(LastFmClientError::InvalidResponse)
        );

        let value = format!("https://www.last.fm/api/auth/?api_key={API_KEY}&token={TOKEN}");
        let authorization = super::DesktopAuthorizationUrl::for_test(&value)
            .expect("valid fixture authorization URL");
        assert_eq!(authorization.as_str(), value);
        assert_eq!(
            format!("{authorization:?}"),
            "DesktopAuthorizationUrl([REDACTED])"
        );
        for invalid in [
            format!("http://www.last.fm/api/auth/?api_key={API_KEY}&token={TOKEN}"),
            format!("https://example.test/api/auth/?api_key={API_KEY}&token={TOKEN}"),
            format!("https://www.last.fm/api/auth/?api_key={API_KEY}"),
            format!("https://www.last.fm/api/auth/?api_key={API_KEY}&token={TOKEN}&token={TOKEN}"),
            format!("https://www.last.fm/api/auth/?api_key={API_KEY}&token=invalid"),
        ] {
            assert_eq!(
                super::DesktopAuthorizationUrl::for_test(&invalid).err(),
                Some(LastFmClientError::InvalidInput)
            );
        }

        let staged = super::DesktopAuthorizedSession::for_test("private-listener", SESSION_KEY)
            .expect("valid staged fixture");
        let rendered = format!("{staged:?}");
        assert!(!rendered.contains("private-listener"));
        assert!(!rendered.contains(SESSION_KEY));
        assert_eq!(
            super::DesktopAuthorizedSession::for_test("line\nbreak", SESSION_KEY).err(),
            Some(LastFmClientError::InvalidResponse)
        );
        assert_eq!(
            super::DesktopAuthorizedSession::for_test("   ", SESSION_KEY).err(),
            Some(LastFmClientError::InvalidResponse)
        );
        assert_eq!(
            super::DesktopAuthorizedSession::for_test("private-listener", "invalid").err(),
            Some(LastFmClientError::InvalidResponse)
        );
    }

    #[test]
    fn auth_success_fields_are_owned_only_by_zeroizing_response_types() {
        let token_body = format!(r#"{{"token":"{TOKEN}"}}"#);
        let response: super::TokenResponse =
            super::parse_auth_response(StatusCode::OK, token_body.as_bytes())
                .expect("valid token envelope");
        assert_eq!(
            format!("{:?}", response.token),
            "SecretResponseString([REDACTED])"
        );
        let token = super::DesktopAuthToken::from_response(response.token)
            .expect("valid token enters protected ownership");
        assert_eq!(format!("{token:?}"), "DesktopAuthToken([REDACTED])");

        let session_body =
            format!(r#"{{"session":{{"name":"private-listener","key":"{SESSION_KEY}"}}}}"#);
        let response: super::SessionEnvelope =
            super::parse_auth_response(StatusCode::OK, session_body.as_bytes())
                .expect("valid session envelope");
        assert_eq!(
            format!("{:?} {:?}", response.session.name, response.session.key),
            "SecretResponseString([REDACTED]) SecretResponseString([REDACTED])"
        );
        let staged = super::DesktopAuthorizedSession::from_response(response.session)
            .expect("valid session remains staged");
        let rendered = format!("{staged:?}");
        assert!(!rendered.contains("private-listener"));
        assert!(!rendered.contains(SESSION_KEY));
    }

    #[test]
    fn escaped_auth_secrets_decode_exactly_into_protected_owners() {
        let escaped_token = TOKEN.replace('0', r"\u0030").replace('f', r"\u0066");
        let token_body = format!(r#"{{"token":"{escaped_token}"}}"#);
        let response: super::TokenResponse =
            super::parse_auth_response(StatusCode::OK, token_body.as_bytes())
                .expect("escaped token envelope");
        let token = super::DesktopAuthToken::from_response(response.token)
            .expect("escaped token validates after direct decoding");
        assert_eq!(token.0.expose(), TOKEN);

        let escaped_key = SESSION_KEY.replace('1', r"\u0031");
        let session_body = format!(
            r#"{{"session":{{"name":"private-\uD83D\uDE80-listener","key":"{escaped_key}"}}}}"#
        );
        let response: super::SessionEnvelope =
            super::parse_auth_response(StatusCode::OK, session_body.as_bytes())
                .expect("escaped session envelope");
        let staged = super::DesktopAuthorizedSession::from_response(response.session)
            .expect("escaped session validates after direct decoding");
        assert_eq!(staged.username(), "private-🚀-listener");

        let (username, key) = staged.into_parts();
        let _: &Zeroizing<String> = &username;
        assert_eq!(username.as_str(), "private-🚀-listener");
        assert_eq!(key.expose(), SESSION_KEY);
    }

    #[test]
    fn partial_escapes_and_unpaired_surrogates_fail_before_auth_interpretation() {
        let invalid_bodies: &[&[u8]] = &[
            br#"{"token":"partial\"#,
            br#"{"token":"\u12"}"#,
            br#"{"token":"\uD800"}"#,
            br#"{"token":"\uDC00"}"#,
            br#"{"token":"\uD800\u0041"}"#,
            br#"{"token":"0123456789abcdef0123456789abcdef","message":"\uD800"}"#,
            br#"{"token":"0123456789abcdef0123456789abcdef","mess\uD800age":"ignored"}"#,
        ];

        for body in invalid_bodies {
            assert_eq!(
                super::parse_auth_response::<super::TokenResponse>(StatusCode::OK, body).err(),
                Some(LastFmClientError::InvalidResponse)
            );
        }
    }

    #[test]
    fn malformed_http_5xx_and_complete_bad_error_members_remain_distinct() {
        let cases: &[(&[u8], LastFmClientError)] = &[
            (br#"{"error":"9""#, LastFmClientError::ServiceUnavailable),
            (
                br#"{"error":"\u12"}"#,
                LastFmClientError::ServiceUnavailable,
            ),
            (
                br#"{"message":"\uD800"}"#,
                LastFmClientError::ServiceUnavailable,
            ),
            (
                br#"{"error":"not-a-code"}"#,
                LastFmClientError::InvalidResponse,
            ),
            (br#"{"error":null}"#, LastFmClientError::InvalidResponse),
            (br#"{"error":[]}"#, LastFmClientError::InvalidResponse),
        ];

        for (body, expected) in cases {
            assert_eq!(
                super::parse_auth_response::<super::TokenResponse>(
                    StatusCode::SERVICE_UNAVAILABLE,
                    body
                )
                .err(),
                Some(*expected)
            );
        }
    }

    #[test]
    fn whitespace_only_decoded_session_username_is_rejected() {
        let body = format!(r#"{{"session":{{"name":"\u0020 \u0020","key":"{SESSION_KEY}"}}}}"#);
        let response: super::SessionEnvelope =
            super::parse_auth_response(StatusCode::OK, body.as_bytes())
                .expect("structurally valid session envelope");
        assert_eq!(
            super::DesktopAuthorizedSession::from_response(response.session).err(),
            Some(LastFmClientError::InvalidResponse)
        );
    }

    #[test]
    fn auth_success_envelopes_require_objects_before_borrowed_deserialization() {
        // Each body is valid JSON. The raw object guard must reject it before
        // serde's derived struct visitor can treat a sequence position or a
        // scalar string as one of the borrowed response fields.
        for body in [format!(r#"["{TOKEN}"]"#), format!(r#""{TOKEN}""#)] {
            let error =
                super::parse_auth_response::<super::TokenResponse>(StatusCode::OK, body.as_bytes())
                    .err()
                    .expect("non-object token envelope must fail");
            assert_eq!(error, LastFmClientError::InvalidResponse);
            assert!(!format!("{error:?} {error}").contains(TOKEN));
        }

        let session_bodies = [
            format!(r#"[{{"name":"private-listener","key":"{SESSION_KEY}"}}]"#),
            r#"{"session":["name","key"]}"#.to_owned(),
            r#""private-session-scalar""#.to_owned(),
            r#"{"session":"private-session-scalar"}"#.to_owned(),
        ];
        for body in session_bodies {
            let error = super::parse_auth_response::<super::SessionEnvelope>(
                StatusCode::OK,
                body.as_bytes(),
            )
            .err()
            .expect("non-object session envelope must fail");
            assert_eq!(error, LastFmClientError::InvalidResponse);
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("private-listener"));
            assert!(!rendered.contains("private-session-scalar"));
            assert!(!rendered.contains(SESSION_KEY));
        }
    }

    #[test]
    fn malformed_partial_and_duplicate_auth_envelopes_fail_closed() {
        let token_envelopes = [
            format!(r#"{{"token":"{TOKEN}","token":"{TOKEN}"}}"#),
            "{}".to_owned(),
            r#"{"token":7}"#.to_owned(),
            format!(r#"{{"token":"{TOKEN}""#),
            format!(r#"{{"token":"{TOKEN}"}} trailing"#),
            format!(r#"[{{"token":"{TOKEN}"}}]"#),
            format!(r#"{{"token":"{TOKEN}","unexpected":"\u0061"}}"#),
        ];
        for body in token_envelopes {
            assert!(matches!(
                super::parse_auth_response::<super::TokenResponse>(StatusCode::OK, body.as_bytes()),
                Err(LastFmClientError::InvalidResponse)
            ));
        }

        let session_envelopes = [
            format!(
                r#"{{"session":{{"name":"first","key":"{SESSION_KEY}"}},"session":{{"name":"second","key":"{SESSION_KEY}"}}}}"#
            ),
            format!(r#"{{"session":{{"name":"first","name":"second","key":"{SESSION_KEY}"}}}}"#),
            format!(
                r#"{{"session":{{"name":"private-listener","key":"{SESSION_KEY}","key":"{SESSION_KEY}"}}}}"#
            ),
            r#"{"session":{"name":"private-listener"}}"#.to_owned(),
            format!(r#"{{"session":{{"key":"{SESSION_KEY}"}}}}"#),
            format!(r#"{{"session":{{"name":7,"key":"{SESSION_KEY}"}}}}"#),
            format!(r#"{{"session":{{"name":"private-listener","key":"{SESSION_KEY}"}}"#),
            format!(
                r#"{{"session":{{"name":"private-listener","key":"{SESSION_KEY}","unexpected":"\u0061"}}}}"#
            ),
            format!(
                r#"{{"session":{{"name":"private-listener","key":"{SESSION_KEY}"}},"unexpected":"\u0061"}}"#
            ),
        ];
        for body in session_envelopes {
            assert!(matches!(
                super::parse_auth_response::<super::SessionEnvelope>(
                    StatusCode::OK,
                    body.as_bytes()
                ),
                Err(LastFmClientError::InvalidResponse)
            ));
        }

        let invalid_token_body = br#"{"token":"not-a-token"}"#;
        let response: super::TokenResponse =
            super::parse_auth_response(StatusCode::OK, invalid_token_body)
                .expect("structurally valid token envelope");
        assert_eq!(
            super::DesktopAuthToken::from_response(response.token).err(),
            Some(LastFmClientError::InvalidResponse)
        );
    }

    #[test]
    fn auth_provider_and_http_classification_matches_generic_policy() {
        let cases: &[(StatusCode, &[u8], LastFmClientError)] = &[
            (
                StatusCode::OK,
                br#"{"error":9,"message":"private-session-text"}"#,
                LastFmClientError::ReauthenticationRequired,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                br#"{"error":"13","message":"private-signature-text"}"#,
                LastFmClientError::ServiceRejected { code: 13 },
            ),
            (
                StatusCode::OK,
                br#"{"error":"\u0038","message":"private-backend-text"}"#,
                LastFmClientError::ServiceUnavailable,
            ),
            (
                StatusCode::OK,
                br#"{"error":29,"message":"private-rate-text"}"#,
                LastFmClientError::RateLimited,
            ),
            (
                StatusCode::OK,
                br#"{"error":28,"message":"future-private-text"}"#,
                LastFmClientError::InvalidResponse,
            ),
            (
                StatusCode::OK,
                br#"{"error":null,"message":"private-null-text"}"#,
                LastFmClientError::InvalidResponse,
            ),
            (
                StatusCode::OK,
                br#"{"error":9,"error":13,"message":"private-duplicate-text"}"#,
                LastFmClientError::InvalidResponse,
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                br#"{"message":"private-rate-status"}"#,
                LastFmClientError::RateLimited,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                br#"{"message":"private-service-status"}"#,
                LastFmClientError::ServiceUnavailable,
            ),
            (
                StatusCode::BAD_REQUEST,
                br#"{"message":"private-http-status"}"#,
                LastFmClientError::HttpStatus,
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                br#"{"message":"private-malformed""#,
                LastFmClientError::ServiceUnavailable,
            ),
            (
                StatusCode::BAD_REQUEST,
                br#""private-scalar""#,
                LastFmClientError::HttpStatus,
            ),
            (
                StatusCode::OK,
                br#""private-scalar""#,
                LastFmClientError::InvalidResponse,
            ),
        ];

        for (status, body, expected) in cases {
            let error = super::parse_auth_response::<super::TokenResponse>(*status, body)
                .err()
                .expect("fixture must fail");
            assert_eq!(&error, expected);
            let rendered = format!("{error:?} {error}");
            for private in [
                "private-session-text",
                "private-signature-text",
                "private-backend-text",
                "private-rate-text",
                "future-private-text",
                "private-null-text",
                "private-duplicate-text",
                "private-rate-status",
                "private-service-status",
                "private-http-status",
                "private-malformed",
                "private-scalar",
            ] {
                assert!(!rendered.contains(private));
            }
        }
    }

    #[tokio::test]
    async fn desktop_auth_flow_consumes_token_and_stages_redacted_session() {
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").replies([
                MockResponse::json(json!({"token": TOKEN})),
                MockResponse::json(json!({
                    "session": {"name": "listener", "key": SESSION_KEY, "subscriber": "0"}
                })),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());

        let token = client.request_auth_token().await.expect("token request");
        let authorization = client.authorization_url(&token).expect("authorization URL");
        assert!(authorization
            .as_str()
            .starts_with("https://www.last.fm/api/auth/?"));
        assert!(authorization.as_str().contains(API_KEY));
        assert!(authorization.as_str().contains(TOKEN));
        assert!(!format!("{token:?} {authorization:?}").contains(TOKEN));
        let _: &super::ProtectedString = &authorization.0;

        // Moving the non-Clone token into this call is the compile-time
        // one-shot boundary; a second exchange cannot be expressed with it.
        let staged = client
            .exchange_auth_token(token)
            .await
            .expect("session exchange");
        assert_eq!(staged.username(), "listener");
        let rendered = format!("{staged:?}");
        assert_eq!(rendered, "DesktopAuthorizedSession([REDACTED])");
        assert!(!rendered.contains("listener"));
        assert!(!rendered.contains(SESSION_KEY));
        let (username, key) = staged.into_parts();
        let _: &Zeroizing<String> = &username;
        assert_eq!(username.as_str(), "listener");
        assert_eq!(key.expose(), SESSION_KEY);

        let requests = service.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(
                request
                    .headers
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/x-www-form-urlencoded")
            );
            assert!(request.headers.get(REFERER).is_none());
            let form = form(&request.body);
            assert_eq!(form.get("api_key").map(String::as_str), Some(API_KEY));
            assert_eq!(form.get("format").map(String::as_str), Some("json"));
            assert_eq!(form.get("api_sig").map(String::len), Some(32));
        }
        assert_eq!(
            form(&requests[0].body).get("method").map(String::as_str),
            Some("auth.getToken")
        );
        assert_eq!(
            form(&requests[1].body).get("token").map(String::as_str),
            Some(TOKEN)
        );
        service.finish().await;
    }

    #[tokio::test]
    async fn now_playing_and_batch_scrobble_preserve_typed_ordered_results() {
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").replies([
                MockResponse::json(json!({"nowplaying": accepted_submission_numeric(1)})),
                MockResponse::json(json!({
                    "scrobbles": {
                        "@attr": {"accepted": "1", "ignored": "1"},
                        "scrobble": [accepted_submission("0"), ignored_submission(3)]
                    }
                })),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());
        let session =
            super::StoredSession::new("listener", super::ProtectedString::new(SESSION_KEY))
                .expect("fixture session");

        assert_eq!(
            client
                .update_now_playing(&session, &track(0))
                .await
                .expect("now-playing update"),
            SubmissionResult::Accepted { corrected: true }
        );
        let result = client
            .scrobble(
                &session,
                &[
                    Scrobble {
                        track: track(0),
                        started_at_unix_seconds: 1_700_000_000,
                    },
                    Scrobble {
                        track: track(1),
                        started_at_unix_seconds: 1_700_000_240,
                    },
                ],
            )
            .await
            .expect("batch scrobble");
        assert_eq!(
            result.items,
            vec![
                SubmissionResult::Accepted { corrected: false },
                SubmissionResult::Ignored {
                    reason: IgnoredReason::TimestampTooOld
                }
            ]
        );

        let requests = service.requests();
        let now_playing = form(&requests[0].body);
        assert_eq!(
            now_playing.get("method").map(String::as_str),
            Some("track.updateNowPlaying")
        );
        assert_eq!(
            now_playing.get("artist").map(String::as_str),
            Some("Artist 0")
        );
        let scrobble = form(&requests[1].body);
        assert_eq!(
            scrobble.get("method").map(String::as_str),
            Some("track.scrobble")
        );
        assert_eq!(
            scrobble.get("artist[0]").map(String::as_str),
            Some("Artist 0")
        );
        assert_eq!(
            scrobble.get("track[1]").map(String::as_str),
            Some("Track 1")
        );
        assert!(!scrobble.contains_key("chosenByUser[1]"));
        assert!(!scrobble.contains_key("mbid[1]"));
        assert_eq!(scrobble.get("sk").map(String::as_str), Some(SESSION_KEY));
        service.finish().await;
    }

    #[tokio::test]
    async fn malformed_batches_and_metadata_fail_before_network_io() {
        let service = MockHttpService::start(Vec::new()).await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());
        let session =
            super::StoredSession::new("listener", super::ProtectedString::new(SESSION_KEY))
                .expect("fixture session");
        assert_eq!(
            client.scrobble(&session, &[]).await,
            Err(LastFmClientError::InvalidInput)
        );
        let oversized = vec![
            Scrobble {
                track: track(0),
                started_at_unix_seconds: 1,
            };
            MAX_SCROBBLES_PER_BATCH + 1
        ];
        assert_eq!(
            client.scrobble(&session, &oversized).await,
            Err(LastFmClientError::InvalidInput)
        );
        let mut invalid_track = track(0);
        invalid_track.artist = "x".repeat(super::MAX_METADATA_BYTES + 1);
        assert_eq!(
            client.update_now_playing(&session, &invalid_track).await,
            Err(LastFmClientError::InvalidInput)
        );
        assert!(service.requests().is_empty());
        service.finish().await;
    }

    #[tokio::test]
    async fn provider_errors_are_typed_without_retaining_messages() {
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").replies([
                MockResponse::json(json!({"error": 9, "message": "session-secret-from-provider"})),
                MockResponse::json(json!({"error": "8", "message": "backend private text"})),
                MockResponse::json(json!({"error": "29", "message": "private metadata"})),
                MockResponse::json(json!({"error": 13, "message": "signature secret"})),
                MockResponse::json(json!({"error": 28, "message": "future provider code"})),
                MockResponse::json(json!({"error": 0, "message": "impossible provider code"})),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());

        let errors = [
            client.request_auth_token().await.expect_err("code 9 fails"),
            client
                .request_auth_token()
                .await
                .expect_err("code 8 fails transiently"),
            client
                .request_auth_token()
                .await
                .expect_err("code 29 fails transiently"),
            client
                .request_auth_token()
                .await
                .expect_err("code 13 fails"),
            client
                .request_auth_token()
                .await
                .expect_err("unknown code fails closed"),
            client
                .request_auth_token()
                .await
                .expect_err("zero code fails closed"),
        ];
        assert_eq!(errors[0], LastFmClientError::ReauthenticationRequired);
        assert!(errors[0].requires_reauthentication());
        assert_eq!(errors[1], LastFmClientError::ServiceUnavailable);
        assert!(errors[1].is_retryable());
        assert_eq!(errors[2], LastFmClientError::RateLimited);
        assert!(errors[2].is_retryable());
        assert_eq!(errors[3], LastFmClientError::ServiceRejected { code: 13 });
        assert_eq!(errors[4], LastFmClientError::InvalidResponse);
        assert_eq!(errors[5], LastFmClientError::InvalidResponse);
        let rendered = format!("{errors:?}");
        assert!(!rendered.contains("provider"));
        assert!(!rendered.contains("private metadata"));
        assert!(!rendered.contains("signature secret"));
        service.finish().await;
    }

    #[test]
    fn published_transient_provider_codes_are_exhaustively_retryable() {
        for (code, expected) in [
            (8, LastFmClientError::ServiceUnavailable),
            (11, LastFmClientError::ServiceUnavailable),
            (16, LastFmClientError::ServiceUnavailable),
            (29, LastFmClientError::RateLimited),
        ] {
            assert_eq!(
                provider_error(&json!({"error": code})).unwrap(),
                Some(expected),
                "published provider code {code} changed retry class"
            );
            assert!(expected.is_retryable());
        }
    }

    #[tokio::test]
    async fn response_body_and_deadline_are_bounded() {
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").replies([
                MockResponse::text("x".repeat(65)),
                MockResponse::json(json!({"token": TOKEN})).with_delay(Duration::from_millis(80)),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::with_test_policy(
            &endpoint,
            credentials(),
            RequestPolicy {
                timeout: Duration::from_millis(25),
                maximum_response_bytes: 64,
                maximum_form_bytes: MAX_FORM_BODY_BYTES,
            },
        );
        assert_eq!(
            client.request_auth_token().await.err(),
            Some(LastFmClientError::BodyLimit)
        );
        assert_eq!(
            client.request_auth_token().await.err(),
            Some(LastFmClientError::Timeout)
        );
        service.finish().await;
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_never_followed() {
        let destination = MockHttpService::start(Vec::new()).await;
        let location = format!("{}/capture", destination.base_url());
        let source = MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").reply(
            MockResponse::status(StatusCode::TEMPORARY_REDIRECT).with_header(
                LOCATION,
                HeaderValue::from_str(&location).expect("redirect header"),
            ),
        )])
        .await;
        let endpoint = format!("{}/2.0/", source.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());
        assert_eq!(
            client.request_auth_token().await.err(),
            Some(LastFmClientError::HttpStatus)
        );
        assert!(destination.requests().is_empty());
        assert!(source.requests()[0].headers.get(REFERER).is_none());
        source.finish().await;
        destination.finish().await;
    }

    #[tokio::test]
    async fn inconsistent_or_malformed_success_responses_fail_closed() {
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").replies([
                MockResponse::json(json!({"token": "not-a-token"})),
                MockResponse::json(json!({"unexpected": true})),
                MockResponse::json(json!({"error": "not-a-code", "token": TOKEN})),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());
        assert_eq!(
            client.request_auth_token().await.err(),
            Some(LastFmClientError::InvalidResponse)
        );
        assert_eq!(
            client.request_auth_token().await.err(),
            Some(LastFmClientError::InvalidResponse)
        );
        assert_eq!(
            client.request_auth_token().await.err(),
            Some(LastFmClientError::InvalidResponse)
        );
        service.finish().await;
    }

    #[tokio::test]
    async fn malformed_session_and_accepted_item_shapes_fail_closed() {
        let service =
            MockHttpService::start(vec![MockRoute::new(Method::POST, "/2.0/").replies([
                MockResponse::json(json!({
                    "session": {"name": "line\nbreak", "key": SESSION_KEY}
                })),
                MockResponse::json(json!({
                    "session": {"name": "private-listener", "key": "not-a-session-key"}
                })),
                MockResponse::json(json!({
                    "nowplaying": {"ignoredMessage": {"code": "0", "#text": ""}}
                })),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());
        let token = super::DesktopAuthToken::for_test(TOKEN).unwrap();
        assert_eq!(
            client.exchange_auth_token(token).await.err(),
            Some(LastFmClientError::InvalidResponse)
        );
        let token = super::DesktopAuthToken::for_test(TOKEN).unwrap();
        assert_eq!(
            client.exchange_auth_token(token).await.err(),
            Some(LastFmClientError::InvalidResponse)
        );
        let session =
            super::StoredSession::new("listener", super::ProtectedString::new(SESSION_KEY))
                .expect("fixture session");
        assert_eq!(
            client.update_now_playing(&session, &track(0)).await.err(),
            Some(LastFmClientError::InvalidResponse)
        );
        service.finish().await;
    }

    #[test]
    fn production_constants_and_policies_are_strict() {
        let client = LastFmClient::new(credentials()).expect("production client");
        assert_eq!(client.endpoint.as_str(), super::API_ENDPOINT);
        assert_eq!(client.endpoint.scheme(), "https");
        assert_eq!(client.endpoint.host_str(), Some("ws.audioscrobbler.com"));
        assert_eq!(
            client.policy.maximum_response_bytes,
            MAX_RESPONSE_BODY_BYTES
        );
        assert_eq!(client.policy.maximum_form_bytes, MAX_FORM_BODY_BYTES);
    }
}
