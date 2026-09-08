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

/// Longest response-body excerpt included in an error message. Keeps a
/// stray HTML error page from flooding the terminal.
const MAX_BODY_EXCERPT: usize = 500;

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
