//! Client for the server's repository event stream,
//! `GET /api/{owner}/{name}/events?branches=main,<branch>` (Server-Sent
//! Events; same event names and payloads as the web's branch stream).
//!
//! The stream only *wakes* the daemon — every signal makes it re-check the
//! remote heads, so a missed, duplicated or misclassified event costs at
//! most one extra check. A 404/405 means the server predates the stream
//! (`oak serve`, older oak.space): the daemon polls instead and the stream
//! is retried every ten minutes.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;

/// What the stream tells the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamSignal {
    Connected,
    Disconnected(String),
    /// No stream on this server (or no access to it).
    Unsupported,
    /// main received a push or merge.
    MainMoved,
    /// The watched branch received a push (possibly our own).
    BranchPushed,
    /// The watched branch was merged or closed.
    BranchClosed,
    /// The server dropped events for us; resync.
    Lag,
}

/// Keepalive pings arrive every 15s; this long without a byte means the
/// connection is dead even if TCP has not noticed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(600);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(60);

pub(crate) fn events_url(remote: &str, repo_path: &str, branches: &[&str]) -> String {
    let branches: Vec<String> = branches
        .iter()
        .map(|branch| urlencoding::encode(branch).into_owned())
        .collect();
    format!(
        "{}/api/{repo_path}/events?branches={}",
        remote.trim_end_matches('/'),
        branches.join(",")
    )
}

#[derive(Deserialize)]
struct BranchEvent {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    branch: Option<String>,
}

/// Classify one complete SSE event.
pub(crate) fn classify(event: &str, data: &str, watched: &str) -> Option<StreamSignal> {
    match event {
        "lag" => Some(StreamSignal::Lag),
        "branch" => {
            let parsed: BranchEvent = serde_json::from_str(data).ok()?;
            let branch = parsed.branch.as_deref()?;
            let kind = parsed.kind.as_str();
            if branch == oak_core::DEFAULT_BRANCH {
                matches!(kind, "push" | "merge" | "branch.updated")
                    .then_some(StreamSignal::MainMoved)
            } else if branch == watched {
                match kind {
                    "push" | "branch.updated" | "branch.renamed" => {
                        Some(StreamSignal::BranchPushed)
                    }
                    "merge" | "branch.closed" => Some(StreamSignal::BranchClosed),
                    _ => None,
                }
            } else if kind == "merge" {
                // Merge events name the merged branch; landing it moved main.
                Some(StreamSignal::MainMoved)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Incremental `text/event-stream` parser: feed bytes, get `(event, data)`
/// pairs for every complete event.
#[derive(Default)]
pub(crate) struct EventParser {
    buffer: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl EventParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    let event = std::mem::take(&mut self.event);
                    let data = std::mem::take(&mut self.data).join("\n");
                    events.push((
                        if event.is_empty() {
                            "message".to_string()
                        } else {
                            event
                        },
                        data,
                    ));
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            match field {
                "event" => self.event = value.to_string(),
                "data" => self.data.push(value.to_string()),
                _ => {}
            }
        }
        events
    }
}

/// Keep a stream open for the daemon's lifetime, reconnecting with backoff.
pub(crate) async fn run(
    client: reqwest::Client,
    url: String,
    api_key: Option<String>,
    watched: String,
    tx: mpsc::UnboundedSender<StreamSignal>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut request = client
            .get(&url)
            .header("accept", "text/event-stream")
            .header("cache-control", "no-cache");
        if let Some(key) = &api_key {
            request = request.bearer_auth(key);
        }
        let response = match tokio::time::timeout(Duration::from_secs(30), request.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                if tx
                    .send(StreamSignal::Disconnected(error.to_string()))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                continue;
            }
            Err(_) => {
                if tx
                    .send(StreamSignal::Disconnected("connect timeout".into()))
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                continue;
            }
        };
        let status = response.status().as_u16();
        let is_stream = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        if matches!(status, 401 | 403 | 404 | 405 | 501)
            || (response.status().is_success() && !is_stream)
        {
            if tx.send(StreamSignal::Unsupported).is_err() {
                return;
            }
            tokio::time::sleep(UNSUPPORTED_RETRY).await;
            continue;
        }
        if !response.status().is_success() {
            if tx
                .send(StreamSignal::Disconnected(format!("HTTP {status}")))
                .is_err()
            {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
            continue;
        }
        if tx.send(StreamSignal::Connected).is_err() {
            return;
        }
        backoff = Duration::from_secs(1);
        let mut parser = EventParser::default();
        let mut body = response.bytes_stream();
        let reason = loop {
            match tokio::time::timeout(IDLE_TIMEOUT, body.next()).await {
                Err(_) => break "idle timeout".to_string(),
                Ok(None) => break "stream ended".to_string(),
                Ok(Some(Err(error))) => break error.to_string(),
                Ok(Some(Ok(bytes))) => {
                    for (event, data) in parser.feed(&bytes) {
                        if let Some(signal) = classify(&event, &data, &watched) {
                            if tx.send(signal).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        };
        if tx.send(StreamSignal::Disconnected(reason)).is_err() {
            return;
        }
        // The server caps connections (30 min); reconnect promptly.
        tokio::time::sleep(backoff).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_split_chunks_comments_and_crlf() {
        let mut parser = EventParser::default();
        assert!(parser.feed(b": hello\r\nevent: bra").is_empty());
        let events = parser.feed(b"nch\r\ndata: {\"kind\":\"push\",\r\ndata: \"branch\":\"main\"}\r\n\r\nevent: ping\ndata:\n\n");
        assert_eq!(
            events,
            vec![
                (
                    "branch".to_string(),
                    "{\"kind\":\"push\",\n\"branch\":\"main\"}".to_string()
                ),
                ("ping".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn classify_maps_main_and_watched_branch_events() {
        let event = |kind: &str, branch: &str| {
            format!(r#"{{"kind":"{kind}","branch":"{branch}","commit":null,"actor":null}}"#)
        };
        assert_eq!(
            classify("branch", &event("merge", "main"), "task"),
            Some(StreamSignal::MainMoved)
        );
        assert_eq!(
            classify("branch", &event("push", "task"), "task"),
            Some(StreamSignal::BranchPushed)
        );
        assert_eq!(
            classify("branch", &event("branch.closed", "task"), "task"),
            Some(StreamSignal::BranchClosed)
        );
        assert_eq!(
            classify("branch", &event("branch.activity", "main"), "task"),
            None
        );
        assert_eq!(classify("branch", &event("push", "other"), "task"), None);
        // The server names the merged branch, not main, on a merge.
        assert_eq!(
            classify("branch", &event("merge", "other"), "task"),
            Some(StreamSignal::MainMoved)
        );
        assert_eq!(
            classify("branch", &event("merge", "task"), "task"),
            Some(StreamSignal::BranchClosed)
        );
        assert_eq!(classify("lag", "", "task"), Some(StreamSignal::Lag));
        assert_eq!(classify("ping", "", "task"), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_reports_connect_events_and_unsupported_servers() {
        use wiremock::matchers::{header, method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::http::ensure_crypto_provider();
        let server = MockServer::start().await;
        let body = "event: ping\ndata: {}\n\nevent: branch\ndata: {\"kind\":\"merge\",\"branch\":\"main\",\"commit\":\"abc\",\"actor\":\"x\"}\n\n";
        Mock::given(method("GET"))
            .and(path("/api/acme/demo/events"))
            .and(query_param("branches", "main,task"))
            .and(header("authorization", "Bearer key"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let url = events_url(&server.uri(), "acme/demo", &["main", "task"]);
        let task = tokio::spawn(run(
            crate::http::api_client(),
            url,
            Some("key".to_string()),
            "task".to_string(),
            tx,
        ));
        assert_eq!(rx.recv().await, Some(StreamSignal::Connected));
        assert_eq!(rx.recv().await, Some(StreamSignal::MainMoved));
        assert!(matches!(
            rx.recv().await,
            Some(StreamSignal::Disconnected(_))
        ));
        task.abort();

        // No stream on this server (old oak.space / `oak serve`): 404.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let url = events_url(&server.uri(), "other/repo", &["main"]);
        let task = tokio::spawn(run(crate::http::api_client(), url, None, "t".into(), tx));
        assert_eq!(rx.recv().await, Some(StreamSignal::Unsupported));
        task.abort();
    }

    #[test]
    fn events_url_encodes_each_branch() {
        assert_eq!(
            events_url("https://oak.space/", "acme/demo", &["main", "a b"]),
            "https://oak.space/api/acme/demo/events?branches=main,a%20b"
        );
    }
}
