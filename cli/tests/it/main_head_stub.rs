//! Test support: tiny loopback remotes for the remote-head tests.
//!
//! * [`serve_local_main_as_remote`] answers one question — "what is
//!   `main`'s head?" — with the checkout's own local `main` head, so a
//!   command that verifies its merge target (`oak merge --dry-run`) finds
//!   it current. Every other request gets a 404, the way a server without
//!   that feature would answer.
//! * [`serve`] answers with whatever the test's handler says and logs every
//!   request line, for fault injection and "made no request" assertions.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use oak_core::{MetadataKey, Repository, SqliteRepository};

/// `"METHOD /path"` of every request a stub received, in order.
pub type RequestLog = Arc<Mutex<Vec<String>>>;

/// Point the checkout at `url` as `stub/repo`.
pub fn link(dir: &Path, url: &str) {
    let repo = SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap();
    repo.set_metadata(MetadataKey::RemoteUrl, url).unwrap();
    repo.set_metadata(MetadataKey::RepoOwner, "stub").unwrap();
    repo.set_metadata(MetadataKey::RepoName, "repo").unwrap();
}

/// A stub remote: `handler(method, path)` returns `(status, json body)`.
/// It lives until the test process exits.
pub fn serve(
    handler: impl Fn(&str, &str) -> (u16, serde_json::Value) + Send + 'static,
) -> (String, RequestLog) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log: RequestLog = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            // Drain the headers, then any declared body.
            let mut content_length = 0usize;
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|read| read > 0) && line != "\r\n" {
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut body = vec![0u8; content_length];
            let _ = std::io::Read::read_exact(&mut reader, &mut body);
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default();
            let path = path.split('?').next().unwrap_or_default().to_string();
            seen.lock().unwrap().push(format!("{method} {path}"));
            let (status, body) = handler(&method, &path);
            let body = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, log)
}

/// Link the checkout at `dir` to a stub remote whose `main` is the
/// checkout's current local `main`, and return the remote URL.
pub fn serve_local_main_as_remote(dir: &Path) -> String {
    let head = SqliteRepository::open(&dir.join(".oak/oak.db"))
        .unwrap()
        .get_branch_head("main")
        .unwrap()
        .map(|head| head.to_string());
    let (url, _log) = serve(move |method, path| {
        if method == "GET" && path == "/api/stub/repo" {
            (
                200,
                serde_json::json!({ "owner": "stub", "name": "repo", "head": head }),
            )
        } else {
            (404, serde_json::json!({ "error": "not found" }))
        }
    });
    link(dir, &url);
    url
}
