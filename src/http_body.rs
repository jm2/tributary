//! Bounded response-body collection for finite HTTP API responses.
//!
//! `Content-Length` is only an advisory early-rejection signal. Every body is
//! counted as it is read so missing or dishonest length headers cannot turn a
//! finite API response into unbounded memory growth.

use std::io::Read;
use std::time::{Duration, Instant};

use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

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
        // Keep partially collected response bytes in zeroizing storage. Dropping
        // this future on timeout or cancellation, and every error return below,
        // therefore wipes the bytes accumulated so far. Content-Length remains
        // advisory and never drives allocation; capacity follows only observed
        // chunks through `append_limited`'s wiping swap.
        let mut body = allocate_body();
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
                return Ok(take_body(body));
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
    // Both buffers may contain response data when a read, cap, allocation, or
    // deadline error returns early, so keep them zeroizing until success.
    let mut body = allocate_body();
    let mut observed = 0_u64;
    let mut buffer = Zeroizing::new([0_u8; 16 * 1024]);

    loop {
        if started.elapsed() >= deadline {
            return Err(ResponseBodyError::DeadlineExceeded { deadline });
        }

        let read = match response.read(&mut buffer[..]) {
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
            return Ok(take_body(body));
        }

        append_limited(&mut body, &mut observed, &buffer[..read], max_bytes)?;
    }
}

fn take_body(mut body: Zeroizing<Vec<u8>>) -> Vec<u8> {
    // Success preserves the public `Vec<u8>` API; the now-empty wrapper can
    // drop without wiping the returned allocation.
    std::mem::take(&mut *body)
}

fn allocate_body() -> Zeroizing<Vec<u8>> {
    // Do not treat the untrusted, advisory Content-Length as an allocation
    // request. Even an in-policy declaration can be hundreds of MiB while the
    // actual response is tiny. Starting empty keeps that lie cheap; observed
    // chunks allocate through the controlled copy/wipe/swap path below.
    Zeroizing::new(Vec::new())
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
    body: &mut Zeroizing<Vec<u8>>,
    observed: &mut u64,
    chunk: &[u8],
    max_bytes: u64,
) -> Result<(), ResponseBodyError> {
    append_limited_with_growth_observer(body, observed, chunk, max_bytes, |_, _, _, _, _, _| {})
}

fn append_limited_with_growth_observer<F>(
    body: &mut Zeroizing<Vec<u8>>,
    observed: &mut u64,
    chunk: &[u8],
    max_bytes: u64,
    observe_growth: F,
) -> Result<(), ResponseBodyError>
where
    F: FnOnce(*const u8, usize, usize, &[u8], *const u8, usize),
{
    let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
    let new_observed = observed.checked_add(chunk_len).unwrap_or(u64::MAX);
    if new_observed > max_bytes {
        return Err(ResponseBodyError::TooLarge {
            size: new_observed,
            limit: max_bytes,
        });
    }

    let required = usize::try_from(new_observed).map_err(|_| ResponseBodyError::InvalidLimit {
        requested: max_bytes,
        maximum: isize::MAX as u64,
    })?;
    let maximum = usize::try_from(max_bytes).map_err(|_| ResponseBodyError::InvalidLimit {
        requested: max_bytes,
        maximum: isize::MAX as u64,
    })?;
    if required > body.capacity() {
        grow_body_without_secret_reallocation(
            body,
            required,
            maximum,
            new_observed,
            observe_growth,
        )?;
    }

    // `grow_body_without_secret_reallocation` guarantees enough spare
    // capacity, so extending here cannot reallocate a secret-bearing Vec.
    body.extend_from_slice(chunk);
    *observed = new_observed;
    Ok(())
}

fn grow_body_without_secret_reallocation<F>(
    body: &mut Zeroizing<Vec<u8>>,
    required: usize,
    maximum: usize,
    requested: u64,
    observe_growth: F,
) -> Result<(), ResponseBodyError>
where
    F: FnOnce(*const u8, usize, usize, &[u8], *const u8, usize),
{
    debug_assert!(required > body.capacity());
    debug_assert!(required <= maximum);

    let old_capacity = body.capacity();
    let doubled_capacity = old_capacity.saturating_mul(2).min(maximum);
    let replacement_capacity = required.max(doubled_capacity);
    let mut replacement = Zeroizing::new(Vec::new());
    replacement
        .try_reserve_exact(replacement_capacity)
        .map_err(|_| ResponseBodyError::AllocationFailed { requested })?;

    let old_len = body.len();
    replacement.extend_from_slice(body.as_slice());
    debug_assert!(replacement.capacity() >= required);

    let old_pointer = body.as_ptr();
    let replacement_pointer = replacement.as_ptr();
    let actual_replacement_capacity = replacement.capacity();

    // Make the whole old allocation initialized and explicitly zero it before
    // it can be deallocated. `resize` cannot allocate because its target is the
    // existing capacity. Keeping the full-capacity slice live for the observer
    // also gives tests a safe, deterministic way to verify the wiped bytes.
    body.resize(old_capacity, 0);
    body.as_mut_slice().zeroize();
    observe_growth(
        old_pointer,
        old_len,
        old_capacity,
        body.as_slice(),
        replacement_pointer,
        actual_replacement_capacity,
    );
    body.clear();

    // The old, already-wiped allocation moves into another Zeroizing wrapper
    // and is wiped once more on drop. The replacement has enough capacity for
    // the pending chunk, so neither the swap nor the following append reallocates.
    std::mem::swap(&mut **body, &mut *replacement);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        allocate_body, append_limited, append_limited_with_growth_observer, read_limited,
        read_limited_blocking, reject_declared_length, take_body, validate_limit,
        ResponseBodyError,
    };
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use zeroize::{ZeroizeOnDrop, Zeroizing};

    fn assert_zeroize_on_drop<T: ZeroizeOnDrop>(_: &T) {}

    #[test]
    fn partial_body_and_blocking_read_buffers_zeroize_on_drop() {
        let body = Zeroizing::new(vec![b's'; 8]);
        let read_buffer = Zeroizing::new([b's'; 16 * 1024]);

        assert_zeroize_on_drop(&body);
        assert_zeroize_on_drop(&read_buffer);
    }

    #[test]
    fn successful_body_take_preserves_bytes() {
        let body = Zeroizing::new(b"response bytes".to_vec());

        assert_eq!(take_body(body), b"response bytes");
    }

    #[test]
    fn huge_in_policy_declared_length_stays_advisory_for_a_small_body() {
        let declared_length = 256 * 1024 * 1024;
        reject_declared_length(Some(declared_length), declared_length)
            .expect("an in-policy declaration remains advisory");

        let mut body = allocate_body();
        assert_eq!(body.capacity(), 0);
        let mut observed = 0;
        append_limited(&mut body, &mut observed, b"ok", declared_length)
            .expect("a tiny observed body must not allocate its huge declaration");

        assert_eq!(body.as_slice(), b"ok");
        assert!(body.capacity() < declared_length as usize);
    }

    #[test]
    fn unknown_length_growth_wipes_old_allocation_before_pointer_transition() {
        let mut body = allocate_body();
        let mut observed = 0;

        append_limited(&mut body, &mut observed, b"secret", 128).expect("first chunk must append");
        let old_pointer = body.as_ptr();
        let old_capacity = body.capacity();
        let max_bytes = u64::try_from(
            old_capacity
                .checked_add(1)
                .expect("test allocation must leave representable growth"),
        )
        .expect("test cap must fit");
        let fill = vec![b'x'; old_capacity - body.len()];
        append_limited(&mut body, &mut observed, &fill, max_bytes)
            .expect("filling existing allocation must append");

        let mut growth_observed = false;
        append_limited_with_growth_observer(
            &mut body,
            &mut observed,
            b"!",
            max_bytes,
            |wiped_pointer,
             wiped_len,
             wiped_capacity,
             wiped_allocation,
             replacement_pointer,
             replacement_capacity| {
                growth_observed = true;
                assert_eq!(wiped_pointer, old_pointer);
                assert_eq!(wiped_len, old_capacity);
                assert_eq!(wiped_capacity, old_capacity);
                assert_eq!(wiped_allocation.len(), old_capacity);
                assert!(wiped_allocation.iter().all(|byte| *byte == 0));
                assert_ne!(replacement_pointer, wiped_pointer);
                assert!(replacement_capacity > wiped_capacity);
            },
        )
        .expect("growth within the cap must append");

        assert!(growth_observed);
        assert_ne!(body.as_ptr(), old_pointer);
        assert_eq!(body.len(), old_capacity + 1);
        assert_eq!(&body[..6], b"secret");
        assert_eq!(body[old_capacity], b'!');
    }

    #[test]
    fn false_small_declared_length_uses_the_same_wiping_growth_path() {
        reject_declared_length(Some(4), 128).expect("small declaration is advisory");
        let mut body = allocate_body();
        let mut observed = 0;
        append_limited(&mut body, &mut observed, b"secret", 128)
            .expect("first observed bytes must allocate");
        let initial_capacity = body.capacity();
        let initial_pointer = body.as_ptr();
        let max_bytes = u64::try_from(initial_capacity + 1).expect("test cap must fit");
        let remaining_secret = vec![b's'; initial_capacity - body.len()];

        append_limited(&mut body, &mut observed, &remaining_secret, max_bytes)
            .expect("current allocation must fill");
        let mut growth_observed = false;
        append_limited_with_growth_observer(
            &mut body,
            &mut observed,
            b"x",
            max_bytes,
            |wiped_pointer, wiped_len, wiped_capacity, wiped_allocation, new_pointer, _| {
                growth_observed = true;
                assert_eq!(wiped_pointer, initial_pointer);
                assert_eq!(wiped_len, initial_capacity);
                assert_eq!(wiped_capacity, initial_capacity);
                assert!(wiped_allocation.iter().all(|byte| *byte == 0));
                assert_ne!(new_pointer, wiped_pointer);
            },
        )
        .expect("observed body within the cap must override its declaration");

        assert!(growth_observed);
        assert_eq!(body.len(), initial_capacity + 1);
        assert_eq!(&body[..6], b"secret");
        assert!(body[6..initial_capacity].iter().all(|byte| *byte == b's'));
        assert_eq!(body[initial_capacity], b'x');
    }

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
            let mut body = allocate_body();
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
