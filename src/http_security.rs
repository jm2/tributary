//! Shared hardening for the application's outbound HTTP clients.
//!
//! Clients come in two shapes. Credential-bearing clients talk to a server the
//! user authenticated against and must never carry those credentials anywhere
//! but that exact origin. Public clients talk to third-party services with no
//! credential attached; they may legitimately be redirected across hosts, but
//! they still must not be walked down to plaintext HTTP or leak the requested
//! URL through a `Referer` header.

use reqwest::Url;

use crate::architecture::AdvertisedHttpRoute;

const MAX_REDIRECTS: usize = 10;
const REDACTED: &str = "REDACTED";
const ADVERTISED_ROUTE_ORIGIN_MISMATCH: &str =
    "advertised HTTP route does not match the HTTP client origin";
const INVALID_BASE_URL: &str = "Invalid server URL: use an http:// or https:// base URL with a host and without embedded credentials, a query, or a fragment";

/// Start an asynchronous client builder with credential-safe redirect defaults.
pub fn authenticated_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .referer(false)
        .redirect(authenticated_redirect_policy())
}

/// Start a blocking client builder with credential-safe redirect defaults.
pub fn authenticated_blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .referer(false)
        .redirect(authenticated_redirect_policy())
}

/// Start an asynchronous client builder for requests that carry no credentials.
pub fn public_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .referer(false)
        .redirect(public_redirect_policy())
}

/// Start a blocking client builder for requests that carry no credentials.
pub fn public_blocking_client_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .referer(false)
        .redirect(public_redirect_policy())
}

/// Apply an mDNS-advertised direct route to an asynchronous client builder.
///
/// `resolve_to_addrs` changes only direct connection resolution: it does not
/// rewrite the request URL and does not disable reqwest's configured system or
/// explicit proxies. A selected proxy therefore retains its normal routing
/// semantics, while a direct or `NO_PROXY` request avoids a second DNS lookup.
pub fn apply_advertised_http_route(
    builder: reqwest::ClientBuilder,
    origin: &Url,
    route: Option<&AdvertisedHttpRoute>,
) -> Result<reqwest::ClientBuilder, &'static str> {
    let Some(route) = route else {
        return Ok(builder);
    };
    if !route.matches_origin(origin) {
        return Err(ADVERTISED_ROUTE_ORIGIN_MISMATCH);
    }
    Ok(builder.resolve_to_addrs(route.hostname(), route.addresses()))
}

/// Apply an mDNS-advertised direct route to a blocking client builder.
///
/// This has the same exact-origin and proxy-preserving contract as
/// [`apply_advertised_http_route`].
pub fn apply_advertised_http_route_blocking(
    builder: reqwest::blocking::ClientBuilder,
    origin: &Url,
    route: Option<&AdvertisedHttpRoute>,
) -> Result<reqwest::blocking::ClientBuilder, &'static str> {
    let Some(route) = route else {
        return Ok(builder);
    };
    if !route.matches_origin(origin) {
        return Err(ADVERTISED_ROUTE_ORIGIN_MISMATCH);
    }
    Ok(builder.resolve_to_addrs(route.hostname(), route.addresses()))
}

/// Remove a request URL before an HTTP error is retained or displayed.
///
/// Reqwest errors can include the complete request URL, including credentials
/// carried in its user-info or query string. Removing the URL is safer than
/// relying on every caller to redact each supported authentication scheme.
pub fn strip_request_url(error: reqwest::Error) -> reqwest::Error {
    error.without_url()
}

/// Validate an HTTP base URL before credentials are attached to requests.
pub fn validate_base_url(url: &Url) -> Result<(), &'static str> {
    if url.cannot_be_a_base()
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(INVALID_BASE_URL);
    }
    Ok(())
}

/// Parse and validate user/config supplied base-URL text before it is logged,
/// persisted, or published as a source identity.
///
/// The error is deliberately fixed and never includes `input`, because the
/// rejected value may itself contain a password or bearer token.
pub fn parse_base_url(input: &str) -> Result<Url, &'static str> {
    let url = Url::parse(input).map_err(|_| INVALID_BASE_URL)?;
    validate_base_url(&url)?;
    Ok(url)
}

/// Mask credentials embedded in a URL before it is written to a log.
///
/// In addition to URL user-info, this covers bearer-like query parameters used
/// by Plex, Jellyfin, DAAP, and Subsonic. The short Subsonic keys are redacted
/// only when the companion parameters identify the URL as Subsonic, avoiding
/// false positives on ordinary `s` and `p` parameters.
pub fn redact_url_secrets(uri: &str) -> String {
    let Ok(mut url) = Url::parse(uri) else {
        return uri.to_string();
    };

    let has_user_info = !url.username().is_empty() || url.password().is_some();
    if has_user_info {
        // Parsed HTTP(S) URLs with user-info always support these setters.
        let _ = url.set_username(REDACTED);
        if url.password().is_some() {
            let _ = url.set_password(Some(REDACTED));
        }
    }

    let query_pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let shape = CredentialShape::of(&query_pairs);

    let mut redacted_query = false;
    let query_pairs: Vec<(String, String)> = query_pairs
        .into_iter()
        .map(|(key, value)| {
            if shape.is_sensitive(&key) {
                redacted_query = true;
                (key, REDACTED.to_string())
            } else {
                (key, value)
            }
        })
        .collect();

    if !has_user_info && !redacted_query {
        return uri.to_string();
    }

    if redacted_query {
        url.query_pairs_mut().clear();
        for (key, value) in &query_pairs {
            url.query_pairs_mut().append_pair(key, value);
        }
    }
    url.to_string()
}

/// Which credential-bearing query parameters a URL actually carries.
///
/// The short Subsonic keys are only treated as credentials when their companion
/// parameters identify the URL as Subsonic, so an ordinary `s` or `p` parameter
/// on some other service is not mistaken for a secret.
struct CredentialShape {
    subsonic_token: bool,
    subsonic_password: bool,
}

impl CredentialShape {
    /// Parameters that are a credential wherever they appear.
    const ALWAYS_SENSITIVE: &'static [&'static str] = &["X-Plex-Token", "api_key", "session-id"];

    fn of(query_pairs: &[(String, String)]) -> Self {
        Self {
            subsonic_token: query_pairs.iter().any(|(key, _)| key == "t"),
            subsonic_password: ["p", "u", "c"]
                .into_iter()
                .all(|required| query_pairs.iter().any(|(key, _)| key == required)),
        }
    }

    fn is_sensitive(&self, key: &str) -> bool {
        Self::ALWAYS_SENSITIVE.contains(&key)
            || (self.subsonic_token && matches!(key, "t" | "s"))
            || (self.subsonic_password && key == "p")
    }
}

/// True when a URL carries a credential that must never reach another device.
///
/// This is the test that decides whether a stream has to be proxied before a
/// Chromecast or an MPD daemon is allowed to fetch it. It recognises URL
/// user-info, Plex's `X-Plex-Token`, Jellyfin's `api_key`, DAAP's `session-id`,
/// and Subsonic's token/salt pair — and Subsonic's `p=enc:<hex>`, which is the
/// user's *password*, not a revocable token.
pub fn url_carries_credentials(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return true;
    }

    let query_pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let shape = CredentialShape::of(&query_pairs);
    query_pairs.iter().any(|(key, _)| shape.is_sensitive(key))
}

/// Whether a media URI may be handed directly to a network playback device.
///
/// Deliberately not `Debug`: `Protected` owns the complete credential-bearing
/// URL. Callers must exchange that URL for an opaque, revocable proxy ticket
/// before sending anything to MPD, Chromecast, or another receiver.
pub enum MediaUriSecurity {
    /// No supported credential shape was found. The caller may retain and use
    /// the original input unchanged.
    Direct,
    /// An HTTP(S) URL carrying a supported credential shape.
    Protected(Box<Url>),
    /// A malformed declared HTTP(S) URL, or credentials on a scheme for which
    /// the exact-origin proxy cannot provide its HTTP-specific guarantees.
    Reject,
}

/// Classify a media URI at the boundary before it can reach a network player.
///
/// Ordinary radio URLs, local `file:` URLs, and MPD library paths remain
/// direct. Credential-bearing HTTP(S) URLs must be proxied. A declared but
/// malformed HTTP(S) URL is rejected rather than being mistaken for an MPD
/// path, and a credential on an unsupported scheme is rejected rather than
/// handed to another device.
pub fn classify_media_uri(uri: &str) -> MediaUriSecurity {
    let declares_http = declared_url_scheme(uri)
        .is_some_and(|scheme| matches_ignore_ascii_case(scheme, "http", "https"));
    if declares_http && !declared_scheme_has_authority(uri) {
        return MediaUriSecurity::Reject;
    }

    let parsed = match Url::parse(uri) {
        Ok(parsed) => parsed,
        Err(_) if declares_http || malformed_uri_looks_credentialed(uri) => {
            return MediaUriSecurity::Reject;
        }
        Err(_) => return MediaUriSecurity::Direct,
    };

    if matches!(parsed.scheme(), "http" | "https") {
        if parsed.cannot_be_a_base() || parsed.host().is_none() {
            return MediaUriSecurity::Reject;
        }
        if url_carries_credentials(&parsed) {
            MediaUriSecurity::Protected(Box::new(parsed))
        } else {
            MediaUriSecurity::Direct
        }
    } else if url_carries_credentials(&parsed) {
        MediaUriSecurity::Reject
    } else {
        MediaUriSecurity::Direct
    }
}

fn declared_url_scheme(uri: &str) -> Option<&str> {
    // URL parsers trim surrounding ASCII whitespace. Mirror that behaviour for
    // the pre-parse declaration check so ` HTTP://[bad` cannot fall through as
    // an opaque MPD path after parsing fails.
    let candidate = uri.trim_matches(|character: char| character.is_ascii_whitespace());
    let (scheme, _) = candidate.split_once(':')?;
    let mut bytes = scheme.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some(scheme)
}

fn declared_scheme_has_authority(uri: &str) -> bool {
    uri.trim_matches(|character: char| character.is_ascii_whitespace())
        .split_once(':')
        .is_some_and(|(_, remainder)| remainder.starts_with("//"))
}

fn matches_ignore_ascii_case(value: &str, first: &str, second: &str) -> bool {
    value.eq_ignore_ascii_case(first) || value.eq_ignore_ascii_case(second)
}

fn malformed_uri_looks_credentialed(uri: &str) -> bool {
    // Scheme-relative network references are not absolute upstream URLs and
    // therefore cannot be proxied safely. Still recognize their user-info so
    // they fail closed instead of reaching a network player.
    let candidate = uri.trim_matches(|character: char| character.is_ascii_whitespace());
    let authority = candidate
        .strip_prefix("//")
        .or_else(|| candidate.split_once("://").map(|(_, rest)| rest))
        .and_then(|rest| rest.split(['/', '?', '#']).next());
    if authority.is_some_and(|authority| authority.contains('@')) {
        return true;
    }

    let Some(query) = candidate
        .split_once('?')
        .map(|(_, query)| query.split('#').next().unwrap_or(query))
    else {
        return false;
    };
    let query_pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let shape = CredentialShape::of(&query_pairs);
    query_pairs.iter().any(|(key, _)| shape.is_sensitive(key))
}

fn authenticated_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }

        if attempt
            .previous()
            .last()
            .is_some_and(|previous| same_http_origin(previous, attempt.url()))
        {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

/// Redirect policy for requests that carry no credentials.
///
/// Radio-Browser mirrors, MusicBrainz, and the geolocation providers all
/// redirect across hosts as a matter of course, so the exact-origin rule used
/// for authenticated clients would simply break them. What they must never do
/// is follow a redirect from HTTPS down to plaintext HTTP, which would expose
/// the request — including a user's coarse location — to any network observer.
fn public_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }

        if attempt
            .previous()
            .last()
            .is_some_and(|previous| downgrades_to_plaintext(previous, attempt.url()))
        {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

fn same_http_origin(left: &Url, right: &Url) -> bool {
    matches!(left.scheme(), "http" | "https")
        && left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// True when a redirect walks an HTTPS request down to plaintext.
fn downgrades_to_plaintext(from: &Url, to: &Url) -> bool {
    from.scheme() == "https" && to.scheme() != "https"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    struct StalledResolver {
        calls: Arc<AtomicUsize>,
    }

    impl reqwest::dns::Resolve for StalledResolver {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    fn url(value: &str) -> Url {
        Url::parse(value).expect("test URL must parse")
    }

    #[test]
    fn exact_origin_accepts_default_and_explicit_ports() {
        assert!(same_http_origin(
            &url("https://example.com/start"),
            &url("https://example.com:443/next")
        ));
        assert!(same_http_origin(
            &url("http://example.com:80/start"),
            &url("http://example.com/next")
        ));
    }

    #[test]
    fn exact_origin_rejects_scheme_host_and_port_changes() {
        let origin = url("https://example.com:8443/start");
        assert!(!same_http_origin(
            &origin,
            &url("http://example.com:8443/next")
        ));
        assert!(!same_http_origin(
            &origin,
            &url("https://other.example.com:8443/next")
        ));
        assert!(!same_http_origin(
            &origin,
            &url("https://example.com:9443/next")
        ));
    }

    #[test]
    fn exact_origin_rejects_non_http_schemes() {
        assert!(!same_http_origin(
            &url("file:///tmp/start"),
            &url("file:///tmp/next")
        ));
    }

    #[test]
    fn authenticated_base_urls_reject_opaque_non_http_userinfo_query_and_fragment_inputs() {
        assert!(validate_base_url(&url("https://music.example.test:443/base")).is_ok());
        for unsafe_url in [
            "music.example.test:443",
            "ftp://music.example.test/base",
            "https://user:secret@music.example.test/base",
            "https://music.example.test/base?api_key=secret",
            "https://music.example.test/base#fragment",
        ] {
            let error = validate_base_url(&url(unsafe_url)).expect_err("unsafe base URL");
            assert!(!error.contains("secret"));
            assert!(!error.contains(unsafe_url));
        }

        for unsafe_text in [
            "not a URL with secret-password",
            "https://user:secret@music.example.test/base",
            "https://music.example.test/base?api_key=secret",
            "https://music.example.test/base#fragment",
        ] {
            let error = parse_base_url(unsafe_text).expect_err("unsafe base URL text");
            assert_eq!(error, INVALID_BASE_URL);
            assert!(!error.contains("secret"));
            assert!(!error.contains(unsafe_text));
        }
    }

    #[test]
    fn redacts_supported_query_credentials() {
        let plex =
            redact_url_secrets("https://plex.example/library?X-Plex-Token=plex-secret&other=value");
        assert!(plex.contains("X-Plex-Token=REDACTED"));
        assert!(plex.contains("other=value"));
        assert!(!plex.contains("plex-secret"));

        let jellyfin = redact_url_secrets("https://jellyfin.example/Items?api_key=jellyfin-secret");
        assert!(jellyfin.contains("api_key=REDACTED"));
        assert!(!jellyfin.contains("jellyfin-secret"));

        let daap =
            redact_url_secrets("http://127.0.0.1:3689/databases/1/items?session-id=daap-secret");
        assert!(daap.contains("session-id=REDACTED"));
        assert!(!daap.contains("daap-secret"));
    }

    #[test]
    fn redacts_subsonic_token_salt_and_contextual_password() {
        let token = redact_url_secrets(
            "https://sub.example/rest/ping?t=token-secret&s=salt-secret&u=admin&c=Tributary",
        );
        assert!(token.contains("t=REDACTED"));
        assert!(token.contains("s=REDACTED"));
        assert!(!token.contains("token-secret"));
        assert!(!token.contains("salt-secret"));

        let password = redact_url_secrets(
            "https://sub.example/rest/ping?u=admin&p=enc%3Apassword-secret&c=Tributary",
        );
        assert!(password.contains("p=REDACTED"));
        assert!(!password.contains("password-secret"));
    }

    #[test]
    fn redacts_url_user_info() {
        let redacted =
            redact_url_secrets("https://private-user:private-password@example.com/music?album=one");
        assert_eq!(
            redacted,
            "https://REDACTED:REDACTED@example.com/music?album=one"
        );
        assert!(!redacted.contains("private-user"));
        assert!(!redacted.contains("private-password"));
    }

    #[test]
    fn leaves_unrelated_and_invalid_urls_unchanged() {
        let ordinary = "https://example.com/api?s=search&p=page&limit=50";
        assert_eq!(redact_url_secrets(ordinary), ordinary);
        let invalid = "not a valid url";
        assert_eq!(redact_url_secrets(invalid), invalid);
    }

    #[test]
    fn advertised_route_helpers_reject_mismatched_origins_without_echoing_them() {
        let secret_host = format!("{}.invalid", uuid::Uuid::new_v4());
        let route_origin = url(&format!("https://{secret_host}"));
        let route =
            AdvertisedHttpRoute::new(&route_origin, [SocketAddr::from(([192, 0, 2, 1], 443))])
                .expect("route");
        let target = url("https://other.invalid");

        let error =
            apply_advertised_http_route(authenticated_client_builder(), &target, Some(&route))
                .expect_err("async mismatch");
        assert_eq!(error, ADVERTISED_ROUTE_ORIGIN_MISMATCH);
        assert!(!error.contains(&secret_host));

        let error = apply_advertised_http_route_blocking(
            authenticated_blocking_client_builder(),
            &target,
            Some(&route),
        )
        .expect_err("blocking mismatch");
        assert_eq!(error, ADVERTISED_ROUTE_ORIGIN_MISMATCH);
        assert!(!error.contains(&secret_host));
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).expect("read test request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(bytes).expect("HTTP request must be UTF-8")
    }

    fn spawn_one_response(
        status: &'static str,
        location: Option<SocketAddr>,
    ) -> (SocketAddr, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (request_tx, request_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let request = read_request(&mut stream);
            request_tx.send(request).expect("capture test request");
            let location = location
                .map(|destination| format!("Location: http://{destination}/target\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {status}\r\n{location}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("write test response");
        });
        (address, request_rx)
    }

    #[test]
    fn asynchronous_advertised_route_bypasses_a_stalled_resolver_and_preserves_host() {
        let (address, request_rx) = spawn_one_response("200 OK", None);
        let origin = url(&format!(
            "http://advertised-route.invalid:{}/from-advertisement",
            address.port()
        ));
        let route = AdvertisedHttpRoute::new(&origin, [address]).expect("route");
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = Arc::new(StalledResolver {
            calls: Arc::clone(&calls),
        });

        let runtime = tokio::runtime::Runtime::new().expect("build runtime");
        runtime.block_on(async {
            let builder = authenticated_client_builder()
                .no_proxy()
                .dns_resolver(resolver);
            let client = apply_advertised_http_route(builder, &origin, Some(&route))
                .expect("matching route")
                .build()
                .expect("build routed client");
            let response =
                tokio::time::timeout(Duration::from_secs(2), client.get(origin.clone()).send())
                    .await
                    .expect("advertised route must not stall in DNS")
                    .expect("send through advertised route");
            assert!(response.status().is_success());
        });

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured routed request");
        assert!(request.starts_with("GET /from-advertisement HTTP/1.1\r\n"));
        assert!(request.lines().any(|line| {
            line.eq_ignore_ascii_case(&format!(
                "Host: advertised-route.invalid:{}",
                address.port()
            ))
        }));
        assert!(!request.contains("Host: 127.0.0.1"));
    }

    #[test]
    fn blocking_advertised_route_bypasses_a_stalled_resolver_and_preserves_host() {
        let (address, request_rx) = spawn_one_response("200 OK", None);
        let origin = url(&format!(
            "http://blocking-route.invalid:{}/blocking-advertisement",
            address.port()
        ));
        let route = AdvertisedHttpRoute::new(&origin, [address]).expect("route");
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = Arc::new(StalledResolver {
            calls: Arc::clone(&calls),
        });
        let builder = authenticated_blocking_client_builder()
            .no_proxy()
            .dns_resolver(resolver);
        let client = apply_advertised_http_route_blocking(builder, &origin, Some(&route))
            .expect("matching route")
            .build()
            .expect("build routed client");
        let response = client
            .get(origin.clone())
            .timeout(Duration::from_secs(2))
            .send()
            .expect("send through advertised route");
        assert!(response.status().is_success());

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured routed request");
        assert!(request.starts_with("GET /blocking-advertisement HTTP/1.1\r\n"));
        assert!(request.lines().any(|line| {
            line.eq_ignore_ascii_case(&format!("Host: blocking-route.invalid:{}", address.port()))
        }));
        assert!(!request.contains("Host: 127.0.0.1"));
    }

    #[test]
    fn advertised_route_does_not_disable_an_explicit_proxy() {
        let target = TcpListener::bind("127.0.0.1:0").expect("bind direct target");
        let target_address = target.local_addr().expect("target address");
        let (proxy_address, proxy_rx) = spawn_one_response("200 OK", None);
        let origin = url(&format!(
            "http://proxied-route.invalid:{}/through-proxy",
            target_address.port()
        ));
        let route = AdvertisedHttpRoute::new(&origin, [target_address]).expect("route");
        let proxy =
            reqwest::Proxy::all(format!("http://{proxy_address}")).expect("explicit test proxy");
        let builder = authenticated_blocking_client_builder().proxy(proxy);
        let client = apply_advertised_http_route_blocking(builder, &origin, Some(&route))
            .expect("matching route")
            .build()
            .expect("build proxied client");
        let response = client
            .get(origin.clone())
            .timeout(Duration::from_secs(2))
            .send()
            .expect("send through proxy");
        assert!(response.status().is_success());

        let request = proxy_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("proxy received request");
        assert!(request.starts_with(&format!(
            "GET http://proxied-route.invalid:{}/through-proxy HTTP/1.1\r\n",
            target_address.port()
        )));
        target.set_nonblocking(true).expect("nonblocking target");
        assert!(matches!(
            target.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn follows_same_origin_without_referer_and_preserves_auth_header() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let request = read_request(&mut stream);
                request_tx.send(request).expect("capture request");
                let response = if index == 0 {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{address}/target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                };
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });

        let client = authenticated_blocking_client_builder()
            .default_headers(reqwest::header::HeaderMap::from_iter([
                (
                    reqwest::header::AUTHORIZATION,
                    reqwest::header::HeaderValue::from_static("Bearer secret"),
                ),
                (
                    reqwest::header::HeaderName::from_static("x-test-auth"),
                    reqwest::header::HeaderValue::from_static("secret"),
                ),
            ]))
            .build()
            .expect("build client");
        let response = client
            .get(format!("http://{address}/start?api_key=secret"))
            .send()
            .expect("same-origin redirect must succeed");
        assert!(response.status().is_success());

        let first = request_rx.recv().expect("initial request");
        let second = request_rx.recv().expect("redirected request");
        let first = first.to_ascii_lowercase();
        let second = second.to_ascii_lowercase();
        assert!(first.contains("authorization: bearer secret"));
        assert!(first.contains("x-test-auth: secret"));
        assert!(second.contains("authorization: bearer secret"));
        assert!(second.contains("x-test-auth: secret"));
        assert!(!second.contains("referer:"));
        server.join().expect("join server");
    }

    #[test]
    fn stops_cross_port_redirect_before_sending_credentials() {
        let destination = TcpListener::bind("127.0.0.1:0").expect("bind destination");
        let destination_address = destination.local_addr().expect("destination address");
        let (origin, origin_rx) = spawn_one_response("302 Found", Some(destination_address));
        let client = authenticated_blocking_client_builder()
            .default_headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::HeaderName::from_static("x-test-auth"),
                reqwest::header::HeaderValue::from_static("secret"),
            )]))
            .build()
            .expect("build client");

        let response = client
            .get(format!("http://{origin}/start"))
            .send()
            .expect("cross-origin redirect must stop cleanly");
        assert!(response.status().is_redirection());
        assert!(origin_rx
            .recv()
            .expect("origin request")
            .to_ascii_lowercase()
            .contains("x-test-auth: secret"));
        destination
            .set_nonblocking(true)
            .expect("make destination nonblocking");
        assert!(matches!(
            destination.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn redirect_loop_is_limited_and_error_url_can_be_stripped() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loop server");
        let address = listener.local_addr().expect("loop server address");
        let server = thread::spawn(move || {
            for _ in 0..=MAX_REDIRECTS {
                let (mut stream, _) = listener.accept().expect("accept loop request");
                let _ = read_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{address}/loop?api_key=loop-secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write redirect");
            }
        });
        let client = authenticated_blocking_client_builder()
            .build()
            .expect("build client");
        let error = client
            .get(format!("http://{address}/loop?api_key=loop-secret"))
            .send()
            .expect_err("redirect loop must fail");
        assert!(error.is_redirect());
        assert!(error.url().is_some());
        let stripped = strip_request_url(error);
        assert!(stripped.url().is_none());
        assert!(!stripped.to_string().contains("loop-secret"));
        server.join().expect("join loop server");
    }

    #[test]
    fn strips_url_from_status_error() {
        let (address, request_rx) = spawn_one_response("500 Internal Server Error", None);
        let client = authenticated_blocking_client_builder()
            .build()
            .expect("build client");
        let error = client
            .get(format!("http://user:password@{address}/?api_key=secret"))
            .send()
            .expect("receive error response")
            .error_for_status()
            .expect_err("500 must be an error");
        assert!(error.url().is_some());
        let stripped = strip_request_url(error);
        assert!(stripped.url().is_none());
        let display = stripped.to_string();
        assert!(!display.contains("user"));
        assert!(!display.contains("password"));
        assert!(!display.contains("secret"));
        let _ = request_rx.recv().expect("error request");
    }

    /// This predicate decides whether a stream is safe to hand to a Chromecast
    /// or an MPD daemon. A false negative publishes the user's credential to a
    /// device on the LAN, so the negative cases below matter as much as the
    /// positive ones.
    #[test]
    fn credential_bearing_urls_are_recognized() {
        for carries in [
            "https://plex.test/library/parts/1/a.flac?X-Plex-Token=secret",
            "https://jellyfin.test/Audio/1/stream?api_key=secret",
            "http://daap.test:3689/databases/1/items/2.mp3?session-id=secret",
            // Subsonic token auth.
            "https://sub.test/rest/stream.view?u=me&t=tok&s=salt&c=Tributary&id=1",
            // Subsonic plaintext auth: `p=enc:<hex>` is the user's password.
            "https://sub.test/rest/stream.view?u=me&p=enc%3A70617373&c=Tributary&id=1",
            // Credentials in user-info.
            "https://me:hunter2@music.test/stream.flac",
            "https://me@music.test/stream.flac",
        ] {
            assert!(
                url_carries_credentials(&url(carries)),
                "{carries} carries a credential"
            );
        }
    }

    #[test]
    fn ordinary_urls_are_not_mistaken_for_credentials() {
        for plain in [
            "http://radio.test/stream.mp3",
            "https://radio.test/listen?bitrate=128&format=mp3",
            // `s` and `p` are Subsonic credentials only alongside their
            // companion parameters; on any other service they are ordinary.
            "https://radio.test/search?s=jazz&p=2",
            "https://radio.test/browse?genre=rock",
        ] {
            assert!(
                !url_carries_credentials(&url(plain)),
                "{plain} carries no credential"
            );
        }
    }

    #[test]
    fn media_uri_classifier_protects_every_supported_http_credential_shape() {
        for protected in [
            "https://user:password@music.test/stream.flac",
            "https://plex.test/file.flac?X-Plex-Token=secret",
            "https://jellyfin.test/stream?api_key=secret",
            "http://daap.test/item?session-id=secret",
            "https://sub.test/stream?u=me&t=token&s=salt&c=Tributary",
            "https://sub.test/stream?u=me&p=enc%3Asecret&c=Tributary",
        ] {
            match classify_media_uri(protected) {
                MediaUriSecurity::Protected(url) => {
                    assert!(url_carries_credentials(&url), "{protected}");
                    assert!(matches!(url.scheme(), "http" | "https"));
                }
                MediaUriSecurity::Direct | MediaUriSecurity::Reject => {
                    panic!("supported credential URI was not protected")
                }
            }
        }
    }

    #[test]
    fn media_uri_classifier_keeps_noncredential_media_and_mpd_paths_direct() {
        for direct in [
            "https://radio.test/live.mp3?quality=high",
            "file:///music/track.flac",
            "Albums/Artist/track.flac",
            "//nas/music/track.flac",
        ] {
            assert!(matches!(
                classify_media_uri(direct),
                MediaUriSecurity::Direct
            ));
        }
    }

    #[test]
    fn media_uri_classifier_rejects_ambiguous_or_unsupported_credentials() {
        for rejected in [
            "HTTP://[malformed",
            " https://[malformed ",
            "http:/missing-host",
            "https://?api_key=secret",
            "ftp://user:password@music.test/stream.flac",
            "ftp://music.test/stream?api_key=secret",
            "//music.test/stream?api_key=secret",
            "not an absolute URL?X-Plex-Token=secret",
        ] {
            assert!(
                matches!(classify_media_uri(rejected), MediaUriSecurity::Reject),
                "{rejected}"
            );
        }
    }

    #[test]
    fn public_policy_rejects_only_plaintext_downgrades() {
        assert!(downgrades_to_plaintext(
            &url("https://radio.example/start"),
            &url("http://radio.example/next")
        ));
        // A cross-host hop that stays on HTTPS is how the Radio-Browser and
        // MusicBrainz mirrors actually work, so it must remain allowed.
        assert!(!downgrades_to_plaintext(
            &url("https://radio.example/start"),
            &url("https://mirror.example/next")
        ));
        assert!(!downgrades_to_plaintext(
            &url("http://radio.example/start"),
            &url("http://mirror.example/next")
        ));
    }

    #[test]
    fn public_policy_follows_cross_origin_redirects_without_a_referer() {
        let (destination, destination_rx) = spawn_one_response("200 OK", None);
        let (origin, origin_rx) = spawn_one_response("302 Found", Some(destination));

        let response = public_blocking_client_builder()
            .build()
            .expect("build public client")
            .get(format!("http://{origin}/start"))
            .send()
            .expect("cross-origin redirect must be followed");
        assert!(response.status().is_success());

        let _ = origin_rx.recv().expect("origin request");
        let redirected = destination_rx
            .recv()
            .expect("redirected request")
            .to_ascii_lowercase();
        assert!(!redirected.contains("referer:"));
    }

    #[test]
    fn asynchronous_builder_can_send_requests() {
        let (address, request_rx) = spawn_one_response("200 OK", None);
        let runtime = tokio::runtime::Runtime::new().expect("build runtime");
        runtime.block_on(async {
            let response = authenticated_client_builder()
                .build()
                .expect("build async client")
                .get(format!("http://{address}/"))
                .send()
                .await
                .expect("send async request");
            assert!(response.status().is_success());
        });
        let _ = request_rx.recv().expect("async request");
    }
}
