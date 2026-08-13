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
}
