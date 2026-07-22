//! Bounded Last.fm 2.0 desktop-authentication and scrobbling client.

use std::fmt;
use std::time::Duration;

use md5::{Digest, Md5};
use reqwest::header::CONTENT_TYPE;
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
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
    pub fn from_build() -> Result<Self, LastFmClientError> {
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
pub struct DesktopAuthToken(ProtectedString);

impl DesktopAuthToken {
    fn from_response(value: String) -> Result<Self, LastFmClientError> {
        validate_hex_credential(&value).map_err(|_| LastFmClientError::InvalidResponse)?;
        Ok(Self(ProtectedString::new(value)))
    }
}

impl fmt::Debug for DesktopAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesktopAuthToken([REDACTED])")
    }
}

/// Browser URL that carries an ephemeral desktop auth token.
pub struct DesktopAuthorizationUrl(Url);

impl DesktopAuthorizationUrl {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for DesktopAuthorizationUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DesktopAuthorizationUrl([REDACTED])")
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
    pub async fn request_auth_token(&self) -> Result<DesktopAuthToken, LastFmClientError> {
        let response: TokenResponse = self
            .signed_post(vec![("method".to_string(), "auth.getToken".to_string())])
            .await?;
        DesktopAuthToken::from_response(response.token)
    }

    /// Build the exact HTTPS Last.fm browser-authorization URL.
    pub fn authorization_url(
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
        Ok(DesktopAuthorizationUrl(url))
    }

    /// Exchange a user-authorized desktop token for a durable session.
    pub async fn exchange_auth_token(
        &self,
        token: &DesktopAuthToken,
    ) -> Result<StoredSession, LastFmClientError> {
        let response: SessionEnvelope = self
            .signed_post(vec![
                ("method".to_string(), "auth.getSession".to_string()),
                ("token".to_string(), token.0.expose().to_string()),
            ])
            .await?;
        validate_text(&response.session.name, MAX_USERNAME_BYTES, true)
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        validate_hex_credential(&response.session.key)
            .map_err(|_| LastFmClientError::InvalidResponse)?;
        StoredSession::new(
            response.session.name,
            ProtectedString::new(response.session.key),
        )
        .map_err(|_| LastFmClientError::InvalidResponse)
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

        let encoded = encode_form(&parameters.0);
        if encoded.len() > self.policy.maximum_form_bytes {
            return Err(LastFmClientError::InvalidInput);
        }

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
        let body = read_limited(
            response,
            self.policy.maximum_response_bytes,
            self.policy.timeout,
        )
        .await
        .map_err(map_body_error)?;

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
}

fn encode_form(parameters: &[(String, String)]) -> Zeroizing<String> {
    Zeroizing::new({
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.extend_pairs(parameters.iter().map(|(key, value)| (&**key, &**value)));
        serializer.finish()
    })
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
    Ok(Some(match code {
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
    }))
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

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Deserialize)]
struct SessionEnvelope {
    session: RawSession,
}

#[derive(Deserialize)]
struct RawSession {
    name: String,
    key: String,
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

    use crate::http_test_service::{MockHttpService, MockResponse, MockRoute};

    use super::{
        append_track_parameters, encode_form, provider_error, sign_parameters, AppCredentials,
        IgnoredReason, LastFmClient, LastFmClientError, LastFmTrack, RequestPolicy, Scrobble,
        ScrobblesEnvelope, SubmissionResult, MAX_FORM_BODY_BYTES, MAX_RESPONSE_BODY_BYTES,
        MAX_SCROBBLES_PER_BATCH,
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

        let encoded = encode_form(&parameters);
        assert!(
            encoded.len() > 512 * 1024,
            "fixture must regress the old undersized cap"
        );
        assert!(encoded.len() <= MAX_FORM_BODY_BYTES);
        assert_eq!(
            form(encoded.as_bytes()).get("artist[49]").map(String::len),
            Some(super::MAX_METADATA_BYTES)
        );
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
        let collected =
            super::read_limited(response, MAX_RESPONSE_BODY_BYTES, Duration::from_secs(1))
                .await
                .expect("maximum valid response fits cap");
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

    #[tokio::test]
    async fn desktop_auth_flow_uses_signed_form_posts_and_redacted_values() {
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

        let session = client
            .exchange_auth_token(&token)
            .await
            .expect("session exchange");
        assert_eq!(session.username(), "listener");
        assert_eq!(session.key().expose(), SESSION_KEY);

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
                    "nowplaying": {"ignoredMessage": {"code": "0", "#text": ""}}
                })),
            ])])
            .await;
        let endpoint = format!("{}/2.0/", service.base_url());
        let client = LastFmClient::for_test(&endpoint, credentials());
        let token = super::DesktopAuthToken::from_response(TOKEN.to_owned()).unwrap();
        assert_eq!(
            client.exchange_auth_token(&token).await.err(),
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
