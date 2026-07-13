//! Shared hardening for HTTP clients that carry authentication credentials.

use reqwest::Url;

const MAX_REDIRECTS: usize = 10;
const REDACTED: &str = "REDACTED";

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
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(
            "Invalid server URL: use an http:// or https:// URL without embedded credentials",
        );
    }
    Ok(())
}

/// Mask credentials embedded in a URL before it is written to a log.
///
/// In addition to URL user-info, this covers bearer-like query parameters used
/// by Plex, Jellyfin, DAAP, and Subsonic. The short Subsonic keys are redacted
/// only when the companion parameters identify the URL as Subsonic, avoiding
/// false positives on ordinary `s` and `p` parameters.
pub fn redact_url_secrets(uri: &str) -> String {
    const SENSITIVE_PARAMS: &[&str] = &["X-Plex-Token", "api_key", "session-id"];

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
    let has_subsonic_token = query_pairs.iter().any(|(key, _)| key == "t");
    let has_subsonic_password = ["p", "u", "c"]
        .into_iter()
        .all(|required| query_pairs.iter().any(|(key, _)| key == required));

    let mut redacted_query = false;
    let query_pairs: Vec<(String, String)> = query_pairs
        .into_iter()
        .map(|(key, value)| {
            let sensitive = SENSITIVE_PARAMS.contains(&key.as_str())
                || (has_subsonic_token && matches!(key.as_str(), "t" | "s"))
                || (has_subsonic_password && key == "p");
            if sensitive {
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

fn same_http_origin(left: &Url, right: &Url) -> bool {
    matches!(left.scheme(), "http" | "https")
        && left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

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
    fn authenticated_base_urls_reject_opaque_non_http_and_userinfo_inputs() {
        assert!(validate_base_url(&url("https://music.example.test:443/base")).is_ok());
        for unsafe_url in [
            "music.example.test:443",
            "ftp://music.example.test/base",
            "https://user:secret@music.example.test/base",
        ] {
            let error = validate_base_url(&url(unsafe_url)).expect_err("unsafe base URL");
            assert!(!error.contains("secret"));
            assert!(!error.contains(unsafe_url));
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
