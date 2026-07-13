//! Bounded response-body collection for finite HTTP API responses.
//!
//! `Content-Length` is only an advisory early-rejection signal. Every body is
//! counted as it is read so missing or dishonest length headers cannot turn a
//! finite API response into unbounded memory growth.

use std::io::Read;
use std::time::{Duration, Instant};

use thiserror::Error;

/// Failure while collecting a finite HTTP response body.
#[derive(Debug, Error)]
pub enum ResponseBodyError {
    /// The declared or observed body size exceeded the configured cap.
    #[error("response body too large: reported size {size} bytes exceeds the {limit}-byte cap")]
    TooLarge { size: u64, limit: u64 },

    /// The configured cap cannot be represented safely by a `Vec<u8>`.
    #[error("response body cap {requested} bytes exceeds the supported {maximum}-byte maximum")]
    InvalidLimit { requested: u64, maximum: u64 },

    /// Memory for an otherwise in-policy body could not be reserved.
    #[error("unable to reserve memory for a {requested}-byte response body")]
    AllocationFailed { requested: u64 },

    /// The body did not complete within its wall-clock deadline.
    #[error("response body deadline exceeded after {deadline:?}")]
    DeadlineExceeded { deadline: Duration },

    /// An asynchronous response body failed while being decoded or read.
    #[error("failed to read response body: {0}")]
    Transport(#[source] reqwest::Error),

    /// A blocking response body failed while being decoded or read.
    ///
    /// Only the I/O category is retained because a blocking reqwest error can
    /// otherwise format the complete request URL.
    #[error("failed to read response body ({kind:?})")]
    BlockingTransport { kind: std::io::ErrorKind },
}

/// Collect an asynchronous response body with an observed-byte cap and total
/// body deadline.
///
/// Callers should also set `RequestBuilder::timeout` so DNS, connection setup,
/// headers, and this body read share an end-to-end request deadline. This
/// helper independently bounds the body phase and remains authoritative about
/// the number of bytes accepted.
pub async fn read_limited(
    mut response: reqwest::Response,
    max_bytes: u64,
    deadline: Duration,
) -> Result<Vec<u8>, ResponseBodyError> {
    validate_limit(max_bytes)?;
    reject_declared_length(response.content_length(), max_bytes)?;

    tokio::time::timeout(deadline, async move {
        let started = tokio::time::Instant::now();
        let mut body = Vec::new();
        let mut observed = 0_u64;

        loop {
            if started.elapsed() >= deadline {
                return Err(ResponseBodyError::DeadlineExceeded { deadline });
            }

            let chunk = match response.chunk().await {
                Ok(chunk) => chunk,
                Err(error) if error.is_timeout() => {
                    return Err(ResponseBodyError::DeadlineExceeded { deadline });
                }
                Err(error) => {
                    return Err(ResponseBodyError::Transport(
                        crate::http_security::strip_request_url(error),
                    ));
                }
            };

            if started.elapsed() >= deadline {
                return Err(ResponseBodyError::DeadlineExceeded { deadline });
            }

            let Some(chunk) = chunk else {
                return Ok(body);
            };
            append_limited(&mut body, &mut observed, &chunk, max_bytes)?;
        }
    })
    .await
    .map_err(|_| ResponseBodyError::DeadlineExceeded { deadline })?
}

/// Collect a blocking response body with an observed-byte cap and cooperative
/// wall-clock deadline.
///
/// Blocking reqwest enforces its request timeout on each individual read. The
/// elapsed-time checks here additionally stop an endless body that keeps
/// producing data before that idle timeout. Callers **must** set this deadline
/// (or a shorter one) on `RequestBuilder::timeout`; a synchronous `Read` cannot
/// otherwise be preempted while it is stalled inside the operating system.
pub fn read_limited_blocking(
    mut response: reqwest::blocking::Response,
    max_bytes: u64,
    deadline: Duration,
) -> Result<Vec<u8>, ResponseBodyError> {
    validate_limit(max_bytes)?;
    reject_declared_length(response.content_length(), max_bytes)?;

    let started = Instant::now();
    let mut body = Vec::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        if started.elapsed() >= deadline {
            return Err(ResponseBodyError::DeadlineExceeded { deadline });
        }

        let read = match response.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if started.elapsed() >= deadline || blocking_error_is_timeout(&error) => {
                return Err(ResponseBodyError::DeadlineExceeded { deadline });
            }
            Err(error) => {
                return Err(ResponseBodyError::BlockingTransport { kind: error.kind() });
            }
        };

        if started.elapsed() >= deadline {
            return Err(ResponseBodyError::DeadlineExceeded { deadline });
        }
        if read == 0 {
            return Ok(body);
        }

        append_limited(&mut body, &mut observed, &buffer[..read], max_bytes)?;
    }
}

fn blocking_error_is_timeout(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
}

fn validate_limit(max_bytes: u64) -> Result<(), ResponseBodyError> {
    let maximum = isize::MAX as u64;
    if max_bytes > maximum {
        return Err(ResponseBodyError::InvalidLimit {
            requested: max_bytes,
            maximum,
        });
    }
    Ok(())
}

fn reject_declared_length(declared: Option<u64>, max_bytes: u64) -> Result<(), ResponseBodyError> {
    if let Some(observed) = declared.filter(|declared| *declared > max_bytes) {
        return Err(ResponseBodyError::TooLarge {
            size: observed,
            limit: max_bytes,
        });
    }
    Ok(())
}

fn append_limited(
    body: &mut Vec<u8>,
    observed: &mut u64,
    chunk: &[u8],
    max_bytes: u64,
) -> Result<(), ResponseBodyError> {
    let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
    let new_observed = observed.checked_add(chunk_len).unwrap_or(u64::MAX);
    if new_observed > max_bytes {
        return Err(ResponseBodyError::TooLarge {
            size: new_observed,
            limit: max_bytes,
        });
    }

    body.try_reserve_exact(chunk.len())
        .map_err(|_| ResponseBodyError::AllocationFailed {
            requested: new_observed,
        })?;
    body.extend_from_slice(chunk);
    *observed = new_observed;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        append_limited, read_limited, read_limited_blocking, reject_declared_length,
        validate_limit, ResponseBodyError,
    };
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn async_response(body: Vec<u8>) -> reqwest::Response {
        http::Response::builder()
            .status(200)
            .body(body)
            .expect("test response must build")
            .into()
    }

    fn blocking_response(body: Vec<u8>) -> reqwest::blocking::Response {
        http::Response::builder()
            .status(200)
            .body(body)
            .expect("test response must build")
            .into()
    }

    fn accept_request(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().expect("test client must connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("test read timeout must apply");
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .expect("test write timeout must apply");
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request);
        stream
    }

    fn spawn_endless_server() -> (SocketAddr, mpsc::Sender<()>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener must bind");
        let address = listener.local_addr().expect("listener has address");
        let (stop_tx, stop_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept_request(&listener);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .expect("headers must write");
            for _ in 0..400 {
                if stop_rx.try_recv().is_ok() || stream.write_all(b"1\r\nx\r\n").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        (address, stop_tx, server)
    }

    fn spawn_stalled_server() -> (SocketAddr, mpsc::Sender<()>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener must bind");
        let address = listener.local_addr().expect("listener has address");
        let (stop_tx, stop_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept_request(&listener);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n")
                .expect("headers must write");
            let _ = stop_rx.recv_timeout(Duration::from_secs(2));
        });
        (address, stop_tx, server)
    }

    fn stop_server(stop: mpsc::Sender<()>, server: thread::JoinHandle<()>) {
        let _ = stop.send(());
        server.join().expect("test server must stop");
    }

    #[test]
    fn counted_chunks_override_missing_and_false_small_declared_lengths() {
        for declared in [None, Some(1)] {
            reject_declared_length(declared, 8).expect("small or absent declaration is advisory");
            let mut body = Vec::new();
            let mut observed = 0;
            let error = append_limited(&mut body, &mut observed, &[b'x'; 9], 8)
                .expect_err("observed bytes must enforce the cap");
            assert!(matches!(
                error,
                ResponseBodyError::TooLarge { size: 9, limit: 8 }
            ));
        }
    }

    #[test]
    fn oversized_declared_length_is_rejected_early() {
        let error = reject_declared_length(Some(9), 8)
            .expect_err("oversized declaration must fail before collection");
        assert!(matches!(
            error,
            ResponseBodyError::TooLarge { size: 9, limit: 8 }
        ));
    }

    #[test]
    fn unrepresentable_vec_limit_is_rejected() {
        let error = validate_limit(u64::MAX).expect_err("impossible Vec cap must be rejected");
        assert!(matches!(error, ResponseBodyError::InvalidLimit { .. }));
    }

    #[tokio::test]
    async fn async_reader_accepts_exact_cap() {
        let body = read_limited(async_response(vec![b'x'; 8]), 8, Duration::from_secs(1))
            .await
            .expect("exact cap must be accepted");
        assert_eq!(body, vec![b'x'; 8]);
    }

    #[test]
    fn blocking_reader_accepts_exact_cap() {
        let body =
            read_limited_blocking(blocking_response(vec![b'x'; 8]), 8, Duration::from_secs(1))
                .expect("exact cap must be accepted");
        assert_eq!(body, vec![b'x'; 8]);
    }

    #[tokio::test]
    async fn async_reader_deadlines_an_endless_chunked_body() {
        let (address, stop, server) = spawn_endless_server();
        let response = reqwest::Client::new()
            .get(format!("http://{address}/endless"))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .expect("test request must receive headers");
        let result = read_limited(response, 1024, Duration::from_millis(40)).await;
        stop_server(stop, server);

        assert!(matches!(
            result,
            Err(ResponseBodyError::DeadlineExceeded { .. })
        ));
    }

    #[test]
    fn blocking_reader_deadlines_an_endless_chunked_body() {
        let (address, stop, server) = spawn_endless_server();
        let response = reqwest::blocking::Client::new()
            .get(format!("http://{address}/endless"))
            .timeout(Duration::from_secs(1))
            .send()
            .expect("test request must receive headers");
        let result = read_limited_blocking(response, 1024, Duration::from_millis(40));
        stop_server(stop, server);

        assert!(matches!(
            result,
            Err(ResponseBodyError::DeadlineExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn async_request_timeout_is_classified_as_a_body_deadline() {
        let (address, stop, server) = spawn_stalled_server();
        let response = reqwest::Client::new()
            .get(format!("http://{address}/stalled"))
            .timeout(Duration::from_millis(500))
            .send()
            .await
            .expect("test request must receive headers");
        let result = read_limited(response, 16, Duration::from_secs(1)).await;
        stop_server(stop, server);

        assert!(matches!(
            result,
            Err(ResponseBodyError::DeadlineExceeded { .. })
        ));
    }

    #[test]
    fn blocking_request_timeout_is_classified_as_a_body_deadline() {
        let (address, stop, server) = spawn_stalled_server();
        let response = reqwest::blocking::Client::new()
            .get(format!("http://{address}/stalled"))
            .timeout(Duration::from_millis(500))
            .send()
            .expect("test request must receive headers");
        let result = read_limited_blocking(response, 16, Duration::from_secs(1));
        stop_server(stop, server);

        assert!(matches!(
            result,
            Err(ResponseBodyError::DeadlineExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn async_transport_errors_do_not_retain_the_request_url() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener must bind");
        let address = listener.local_addr().expect("listener has address");
        let secret = uuid::Uuid::new_v4().to_string();
        let server = thread::spawn(move || {
            let mut stream = accept_request(&listener);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nx")
                .expect("truncated response must write");
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}/body?token={secret}"))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .expect("test request must receive headers");
        let result = read_limited(response, 16, Duration::from_secs(1)).await;
        server.join().expect("test server must stop");
        let rendered = result.expect_err("truncated body must fail").to_string();

        assert!(!rendered.contains(&secret));
        assert!(!rendered.contains(&address.to_string()));
    }
}
