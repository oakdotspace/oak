//! Shared HTTP plumbing for talking to an Oak remote.
//!
//! API requests must never follow redirects silently: reqwest's default
//! policy rewrites a redirected POST into a GET (per the HTTP spec for
//! 301/302), so when a host moves (oakvcs.com → oak.space) `POST /push`
//! quietly became a GET against the new origin and died with an opaque
//! 405 — while GET-based commands kept working and masked the move.
//! [`api_client`] disables redirect following, and [`server_error`] turns
//! the resulting 3xx into a structured [`OakError::RemoteMoved`]. When the
//! redirect target is a trusted Oak host (see [`is_trusted_origin`]),
//! `oak push` / `oak pull` catch that error, retarget the repo's stored
//! remote, and retry once; for any other target the error's message tells
//! the user to re-run with `-r <origin>`.

use oak_core::OakError;

/// A mutation request may have committed once it was sent. Transport errors,
/// ambiguous statuses, and invalid receipts therefore cannot be reported as
/// rollback. Keep the reason caller-controlled: remote bodies, URLs, and
/// parser diagnostics can contain credentials or private content.
pub(crate) fn publication_unconfirmed(
    operation: impl Into<String>,
    reason: impl Into<String>,
    reconciliation_commands: Vec<String>,
) -> OakError {
    OakError::PublicationUnconfirmed {
        operation: operation.into(),
        reason: reason.into(),
        reconciliation_commands,
    }
}

/// Status codes that do not prove whether a non-idempotent mutation landed.
/// Redirects are ambiguous here because publication requests are never
/// replayed automatically at another origin.
pub(crate) fn publication_status_is_unconfirmed(status: reqwest::StatusCode) -> bool {
    status.is_redirection()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Longest response-body excerpt included in an error message. Keeps a
/// stray HTML error page from flooding the terminal.
const MAX_BODY_EXCERPT: usize = 500;

const PUBLICATION_RECEIPT_MAX_BYTES: usize = 64 * 1024;
const PUBLICATION_RECEIPT_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const PUBLICATION_REPLY_HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const PUBLICATION_UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum PublicationReceiptReadError {
    #[error("the publication receipt exceeded 65536 bytes")]
    Oversized,
    #[error("the publication receipt body timed out")]
    TimedOut,
    #[error("the publication receipt stream failed")]
    StreamFailed,
}

#[derive(Debug)]
enum PublicationWaitError<E> {
    Transport(E),
    BodySignalLost,
    ReplyTimedOut,
}

async fn wait_for_publication_response<F, T, E>(
    response: F,
    mut body_consumed: tokio::sync::oneshot::Receiver<()>,
    reply_timeout: std::time::Duration,
) -> std::result::Result<T, PublicationWaitError<E>>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    tokio::pin!(response);
    tokio::select! {
        result = &mut response => return result.map_err(PublicationWaitError::Transport),
        consumed = &mut body_consumed => {
            if consumed.is_err() {
                return Err(PublicationWaitError::BodySignalLost);
            }
        }
    }
    tokio::time::timeout(reply_timeout, &mut response)
        .await
        .map_err(|_| PublicationWaitError::ReplyTimedOut)?
        .map_err(PublicationWaitError::Transport)
}

/// Send one non-idempotent publication request without retrying it. The
/// request-body stream remains unbounded, preserving the existing allowance
/// for slow valid uploads. The fixed reply-header wait starts only after Hyper
/// consumes the last body chunk (which is a transport-consumption boundary,
/// not a server acknowledgement). The caller still owns status and receipt-
/// shape interpretation.
pub(crate) async fn send_publication_request(
    request: reqwest::RequestBuilder,
    body: Vec<u8>,
    operation: &str,
    reconciliation_commands: &[String],
) -> oak_core::Result<reqwest::Response> {
    send_publication_request_with_reply_timeout(
        request,
        body,
        operation,
        reconciliation_commands,
        PUBLICATION_REPLY_HEADER_TIMEOUT,
    )
    .await
}

pub(crate) async fn send_publication_request_with_reply_timeout(
    request: reqwest::RequestBuilder,
    body: Vec<u8>,
    operation: &str,
    reconciliation_commands: &[String],
    reply_timeout: std::time::Duration,
) -> oak_core::Result<reqwest::Response> {
    let body_len = body.len();
    let body = std::sync::Arc::new(body);
    let (body_consumed_tx, body_consumed_rx) = tokio::sync::oneshot::channel();
    let request = if body_len == 0 {
        // Hyper does not poll an empty body stream when Content-Length is
        // zero, so report completion directly for bodyless mutations.
        let _ = body_consumed_tx.send(());
        request.body(Vec::new())
    } else {
        let stream = futures_util::stream::unfold(
            (body, 0usize, Some(body_consumed_tx)),
            |(body, offset, mut body_consumed_tx)| async move {
                if offset >= body.len() {
                    return None;
                }
                let end = offset
                    .saturating_add(PUBLICATION_UPLOAD_CHUNK_BYTES)
                    .min(body.len());
                let chunk = body[offset..end].to_vec();
                if end == body.len() {
                    if let Some(body_consumed_tx) = body_consumed_tx.take() {
                        let _ = body_consumed_tx.send(());
                    }
                }
                Some((
                    Ok::<Vec<u8>, std::io::Error>(chunk),
                    (body, end, body_consumed_tx),
                ))
            },
        );
        request.body(reqwest::Body::wrap_stream(stream))
    };
    let response = request
        .header(reqwest::header::CONTENT_LENGTH, body_len.to_string())
        .send();
    wait_for_publication_response(
        response,
        body_consumed_rx,
        reply_timeout,
    )
    .await
    .map_err(|error| {
        let reason = match error {
            PublicationWaitError::Transport(_) => {
                "the response headers were not received after the request was sent"
            }
            PublicationWaitError::BodySignalLost => {
                "request body transmission could not be tracked"
            }
            PublicationWaitError::ReplyTimedOut => {
                "response headers did not arrive within the bounded wait after the request body was consumed"
            }
        };
        publication_unconfirmed(
            operation,
            reason,
            reconciliation_commands.to_vec(),
        )
    })
}

/// Consume a mutation receipt only after response headers have arrived. The
/// fixed timeout therefore never includes request-body upload time. Both the
/// declared and streamed sizes are bounded, and transport diagnostics are
/// deliberately discarded because they can contain credential-bearing URLs.
pub(crate) async fn bounded_publication_body(
    response: reqwest::Response,
) -> std::result::Result<Vec<u8>, PublicationReceiptReadError> {
    bounded_publication_body_with_timeout(response, PUBLICATION_RECEIPT_BODY_TIMEOUT).await
}

async fn bounded_publication_body_with_timeout(
    mut response: reqwest::Response,
    timeout: std::time::Duration,
) -> std::result::Result<Vec<u8>, PublicationReceiptReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > PUBLICATION_RECEIPT_MAX_BYTES as u64)
    {
        return Err(PublicationReceiptReadError::Oversized);
    }
    tokio::time::timeout(timeout, async move {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| PublicationReceiptReadError::StreamFailed)?
        {
            if chunk.len() > PUBLICATION_RECEIPT_MAX_BYTES.saturating_sub(body.len()) {
                return Err(PublicationReceiptReadError::Oversized);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    })
    .await
    .map_err(|_| PublicationReceiptReadError::TimedOut)?
}

/// User-Agent sent on every request the CLI makes. reqwest sends no
/// User-Agent by default, and Cloudflare's bot rules discriminate against
/// UA-less requests (a byte-identical request with a UA succeeds where the
/// bare one 404s) — so every client the CLI constructs must set this.
pub const USER_AGENT: &str = concat!("oak-cli/", env!("CARGO_PKG_VERSION"));

/// Client for Oak API requests. Never follows redirects — a 3xx response
/// reaches the caller's status check, where [`server_error`] reports the
/// moved remote instead of replaying the request (as a GET) elsewhere.
///
/// One process-wide client: constructing a fresh `reqwest::Client` per call
/// gave every logical phase its own empty connection pool, so a multi-step
/// flow (mount startup's resolve-HEAD → manifest → blob-metadata, push's
/// head-check → dedup → upload) paid a new TCP+TLS+h2 handshake (~70-90ms
/// against oak.space) per step and closed the old connection behind it.
/// `reqwest::Client` is an `Arc` around its pool, so cloning the shared one
/// is cheap and every step reuses the warm connection. A pooled connection
/// whose driver task died (e.g. its tokio runtime ended — the CLI builds
/// more than one) is evicted on checkout and replaced with a fresh dial,
/// the same cost as the old per-call behavior.
/// Install the process-default rustls [`CryptoProvider`] exactly once.
///
/// reqwest is built on `rustls-no-provider` (see the workspace Cargo.toml — we
/// keep aws-lc-sys out of the build so the CLI compiles with no C/NASM
/// toolchain, on Windows in particular). The trade-off is that constructing
/// *any* `reqwest::Client` panics with "no process-level CryptoProvider
/// available" unless a provider was installed first. `main()` installs it at
/// startup, but library code paths and tests build clients without going
/// through `main`, so every client constructor calls this first. Idempotent
/// and cheap after the first call.
///
/// [`CryptoProvider`]: rustls::crypto::CryptoProvider
pub fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err means a provider is already installed — fine, that's the goal.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn api_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            ensure_crypto_provider();
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("building a reqwest client with a redirect policy cannot fail")
        })
        .clone()
}

const IDEMPOTENT_MAX_ATTEMPTS: usize = 5;
const IDEMPOTENT_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);

pub fn retryable_idempotent_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

fn retry_after(response: &reqwest::Response) -> std::time::Duration {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or_default()
}

/// Replay an idempotent API request across transient admission/upstream
/// failures. The caller owns the overall deadline so a retry can never make a
/// clone, pull, or push wait forever. Mutating publication requests must not
/// use this helper.
pub async fn send_idempotent_with_retry_until(
    request: reqwest::RequestBuilder,
    context: &str,
    deadline: tokio::time::Instant,
) -> oak_core::Result<reqwest::Response> {
    let template = request.try_clone().ok_or_else(|| {
        OakError::InvalidArgument(format!("{context} request body cannot be replayed safely"))
    })?;
    let mut backoff = std::time::Duration::from_millis(100);
    let mut last_error = String::new();
    for attempt_index in 0..IDEMPOTENT_MAX_ATTEMPTS {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let attempt = template.try_clone().ok_or_else(|| {
            OakError::InvalidArgument(format!("{context} request body cannot be replayed safely"))
        })?;
        match attempt.timeout(remaining).send().await {
            Ok(response) if retryable_idempotent_status(response.status()) => {
                last_error = format!("HTTP {}", response.status());
                if attempt_index + 1 == IDEMPOTENT_MAX_ATTEMPTS {
                    break;
                }
                let wait = retry_after(&response)
                    .max(backoff)
                    .min(IDEMPOTENT_MAX_BACKOFF);
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if wait >= remaining {
                    break;
                }
                tokio::time::sleep(wait).await;
                backoff = (backoff * 2).min(IDEMPOTENT_MAX_BACKOFF);
            }
            Ok(response) => return Ok(response),
            Err(error) => {
                last_error = error.to_string();
                if attempt_index + 1 == IDEMPOTENT_MAX_ATTEMPTS {
                    break;
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if backoff >= remaining {
                    break;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(IDEMPOTENT_MAX_BACKOFF);
            }
        }
    }
    Err(OakError::Server(format!(
        "{context} exhausted its bounded idempotent retry budget: {last_error}"
    )))
}

/// Built-in origins the CLI may retarget a repo's remote to automatically
/// when the old host redirects there. Anything else requires an explicit
/// `oak push -r <origin>` from the user.
const TRUSTED_ORIGINS: &[&str] = &["https://oak.space"];

/// Whether `origin` (`scheme://host[:port]`) is a known Oak host that a
/// moved remote may be auto-updated to. `OAK_TRUSTED_REMOTES` — a
/// comma-separated list of origins — extends the built-in list; it exists
/// for tests, where the "new" host is a local mock server.
pub fn is_trusted_origin(origin: &str) -> bool {
    let origin = origin.trim_end_matches('/');
    if TRUSTED_ORIGINS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(origin))
    {
        return true;
    }
    std::env::var("OAK_TRUSTED_REMOTES").is_ok_and(|extra| {
        extra
            .split(',')
            .map(|t| t.trim().trim_end_matches('/'))
            .any(|t| !t.is_empty() && t.eq_ignore_ascii_case(origin))
    })
}

/// Convert a non-success response into an `OakError`, always naming the
/// HTTP status (and body, when present) — `Server error:` with nothing
/// after the colon must never happen. A redirect to another origin becomes
/// the structured [`OakError::RemoteMoved`] so callers can follow a
/// trusted host move instead of just printing it.
pub async fn server_error(resp: reqwest::Response) -> OakError {
    if resp.status().is_redirection() {
        let origin = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(origin_from_location);
        if let Some(origin) = origin {
            return OakError::RemoteMoved { origin };
        }
    }
    OakError::Server(error_text(resp).await)
}

/// The message body for [`server_error`], for call sites that wrap it in
/// their own context (`format!("Failed to check chunks: {}", ...)`).
pub async fn error_text(resp: reqwest::Response) -> String {
    let status = resp.status();
    if status.is_redirection() {
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok());
        if let Some(origin) = location.and_then(origin_from_location) {
            return OakError::RemoteMoved { origin }.to_string();
        }
        return match location {
            Some(loc) => format!("HTTP {status} (unexpected redirect to {loc})"),
            None => format!("HTTP {status} (redirect with no Location header)"),
        };
    }
    let body = resp.text().await.unwrap_or_default();
    let body = match json_error_message(body.trim()) {
        Some(msg) => msg,
        None => body.trim().to_string(),
    };
    let body = body.as_str();
    if body.is_empty() {
        format!("HTTP {status}")
    } else if body.len() > MAX_BODY_EXCERPT {
        let cut = body
            .char_indices()
            .take_while(|(i, _)| *i < MAX_BODY_EXCERPT)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!("HTTP {status}: {}…", &body[..cut])
    } else {
        format!("HTTP {status}: {body}")
    }
}

/// The `error` field of a JSON `{"error":"..."}` body, when that's what the
/// body is. The server wraps every error in that envelope; unwrapping it
/// here keeps raw JSON out of user-facing messages.
pub fn json_error_message(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        error: String,
    }
    serde_json::from_str::<ErrorBody>(body)
        .ok()
        .map(|b| b.error)
        .filter(|msg| !msg.trim().is_empty())
}

/// Extract `scheme://host[:port]` from a `Location` header value. Returns
/// `None` for relative redirects — those don't indicate a host move.
fn origin_from_location(location: &str) -> Option<String> {
    let url = reqwest::Url::parse(location).ok()?;
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{}://{host}:{port}", url.scheme())),
        None => Some(format!("{}://{host}", url.scheme())),
    }
}

#[cfg(test)]
mod tests {
    use super::origin_from_location;

    #[tokio::test(flavor = "current_thread")]
    async fn publication_reply_timeout_starts_after_body_consumption() {
        let (consumed_tx, consumed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let _ = consumed_tx.send(());
        });
        let response = async {
            tokio::time::sleep(std::time::Duration::from_millis(65)).await;
            Ok::<_, ()>("receipt")
        };
        assert_eq!(
            super::wait_for_publication_response(
                response,
                consumed_rx,
                std::time::Duration::from_millis(40),
            )
            .await
            .unwrap(),
            "receipt"
        );

        let (consumed_tx, consumed_rx) = tokio::sync::oneshot::channel();
        consumed_tx.send(()).unwrap();
        let stalled = async {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            Ok::<_, ()>("late")
        };
        assert!(matches!(
            super::wait_for_publication_response(
                stalled,
                consumed_rx,
                std::time::Duration::from_millis(20),
            )
            .await,
            Err(super::PublicationWaitError::ReplyTimedOut)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_request_reader_does_not_spend_the_reply_header_budget() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let header_end = loop {
                let mut chunk = vec![0u8; 64 * 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                received.extend_from_slice(&chunk[..read]);
                if let Some(offset) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break offset + 4;
                }
            };
            let headers = String::from_utf8_lossy(&received[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();

            // Hold back reads longer than the reply-header budget. A timer
            // that starts before body-stream backpressure clears will fire.
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            let mut body_read = received.len() - header_end;
            let mut chunk = vec![0u8; 64 * 1024];
            while body_read < content_length {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                body_read += read;
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });

        let body = vec![b'x'; 16 * 1024 * 1024];
        let response = super::send_publication_request_with_reply_timeout(
            super::api_client().post(format!("http://{address}/publish")),
            body,
            "test publication",
            &[],
            std::time::Duration::from_millis(30),
        )
        .await
        .expect("slow body transmission must not consume the reply-header budget");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn accepted_request_with_lost_reply_is_unconfirmed_and_not_replayed() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let header_end = loop {
                let mut chunk = vec![0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                received.extend_from_slice(&chunk[..read]);
                if let Some(offset) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break offset + 4;
                }
            };
            let headers = String::from_utf8_lossy(&received[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap();
            while received.len() - header_end < content_length {
                let mut chunk = vec![0u8; 4096];
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                received.extend_from_slice(&chunk[..read]);
            }
            accepted_tx
                .send(received[header_end..header_end + content_length].to_vec())
                .unwrap();
            // Drop the connection after accepting the full request and before
            // sending response headers: the client cannot infer rollback.
        });

        let body = br#"{"target":"abc"}"#.to_vec();
        let result = super::send_publication_request(
            super::api_client().post(format!("http://{address}/publish")),
            body.clone(),
            "test publication",
            &["oak branch show feature --remote --json".to_string()],
        )
        .await;
        assert_eq!(accepted_rx.await.unwrap(), body);
        assert!(matches!(
            result,
            Err(oak_core::OakError::PublicationUnconfirmed { .. })
        ));
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_never_accepted_is_still_reported_as_unconfirmed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let result = super::send_publication_request(
            super::api_client().post(format!("http://{address}/publish")),
            b"request".to_vec(),
            "test publication",
            &["oak branch show feature --remote --json".to_string()],
        )
        .await;
        assert!(matches!(
            result,
            Err(oak_core::OakError::PublicationUnconfirmed { .. })
        ));
    }

    #[test]
    fn origin_extracted_from_absolute_location() {
        assert_eq!(
            origin_from_location("https://oak.space/api/acme/blog/push").as_deref(),
            Some("https://oak.space")
        );
    }

    #[test]
    fn origin_keeps_explicit_port() {
        assert_eq!(
            origin_from_location("http://localhost:8080/api").as_deref(),
            Some("http://localhost:8080")
        );
    }

    #[test]
    fn relative_location_is_not_a_host_move() {
        assert_eq!(origin_from_location("/login"), None);
    }

    #[test]
    fn json_error_envelope_is_unwrapped() {
        assert_eq!(
            super::json_error_message(r#"{"error":"Merge conflict: 1 file(s)"}"#).as_deref(),
            Some("Merge conflict: 1 file(s)")
        );
    }

    #[test]
    fn non_json_and_empty_error_bodies_pass_through() {
        assert_eq!(super::json_error_message("<html>502</html>"), None);
        assert_eq!(super::json_error_message(r#"{"error":""}"#), None);
        assert_eq!(super::json_error_message(r#"{"detail":"x"}"#), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idempotent_request_replays_exact_body_after_rate_limit() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!({"hashes": ["a".repeat(64)]});
        Mock::given(method("POST"))
            .and(path("/chunks/download"))
            .and(body_json(body.clone()))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "0")
                    .set_body_string("busy"),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chunks/download"))
            .and(body_json(body.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "chunks": []
            })))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;

        let response = super::send_idempotent_with_retry_until(
            super::api_client()
                .post(format!("{}/chunks/download", server.uri()))
                .json(&body),
            "chunk download",
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idempotent_request_never_retries_auth_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        let response = super::send_idempotent_with_retry_until(
            super::api_client().post(format!("{}/private", server.uri())),
            "private read",
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
    }
}
