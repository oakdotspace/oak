//! Sharing uncommitted work: the `watch.share_working` side of the daemon.
//!
//! When the setting is on, the daemon sends the checkout's changed text files
//! to `PUT /api/:owner/:name/branches/:branch/working`, and oak.space shows
//! them on the branch page. The server holds them in memory and drops them
//! unless they are resent, so the daemon resends unchanged work as a
//! keepalive. This module builds the snapshot and sends it; deciding when is
//! `Daemon::maybe_share_working`.
//!
//! Anything sent here leaves the machine before anyone chose to publish it,
//! so the setting is off by default and the scan stays narrow: only what
//! `oak status` reports (so never an ignored file), only regular text files
//! within the server's limits, and never a file whose name looks like a
//! credential, whether or not it is ignored.

use std::path::Path;
use std::time::Duration;

use oak_core::{ChangeType, FileChange, Hash, Repository, Result, SqliteRepository};
use serde::Serialize;

/// The server's per-file limit, shared with code search.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// The server's per-snapshot limits.
const MAX_FILES: usize = 200;
const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CLIENT_CHARS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SharedFile {
    pub path: String,
    /// Full file text; `None` (JSON `null`) means the file was deleted.
    pub text: Option<String>,
}

/// One scan of the checkout.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub files: Vec<SharedFile>,
    /// Changed files left out: binary, too large, or credential-shaped.
    pub skipped: usize,
}

impl Snapshot {
    /// Identifies this content, so a rescan can tell whether anything changed.
    pub fn fingerprint(&self, head: &str) -> String {
        let body = serde_json::to_vec(&self.files).unwrap_or_default();
        let mut bytes = head.as_bytes().to_vec();
        bytes.extend_from_slice(&body);
        oak_core::hash_bytes(&bytes).to_string()
    }
}

/// Does this name look like it holds a credential? This backs up
/// `.oakignore` for the common cases, because a file that slipped past it is
/// sent the moment it's saved. It errs towards leaving files out.
fn looks_like_a_secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if name == ".env" || name.starts_with(".env.") || name.ends_with(".env") {
        return !matches!(
            name.as_str(),
            ".env.example" | ".env.sample" | ".env.template"
        );
    }
    const NAMES: &[&str] = &[
        ".npmrc",
        ".pypirc",
        ".netrc",
        ".pgpass",
        "credentials",
        "credentials.json",
        "secrets.json",
        "secrets.yaml",
        "secrets.yml",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ];
    const SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"];
    NAMES.contains(&name.as_str()) || SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// The text of a changed file, or `None` when it can't be shared.
fn shareable_text(work_tree: &Path, path: &str) -> Option<String> {
    let full = work_tree.join(path);
    // Not `metadata`: a symlink could point anywhere on the machine.
    let meta = std::fs::symlink_metadata(&full).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(&full).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Turn a status change set into a snapshot, within the server's limits.
pub(crate) fn build_snapshot(work_tree: &Path, changes: &[FileChange]) -> Snapshot {
    let mut wanted: Vec<(String, bool)> = Vec::with_capacity(changes.len());
    for change in changes {
        match change.change_type {
            ChangeType::Deleted => wanted.push((change.path.clone(), false)),
            ChangeType::Added | ChangeType::Modified => wanted.push((change.path.clone(), true)),
            ChangeType::Renamed => {
                wanted.push((change.path.clone(), true));
                if let Some(old) = &change.old_path {
                    wanted.push((old.clone(), false));
                }
            }
        }
    }
    wanted.sort();
    wanted.dedup_by(|a, b| a.0 == b.0);

    let mut snapshot = Snapshot::default();
    let mut bytes = 0u64;
    for (path, exists) in wanted {
        if looks_like_a_secret(&path) || snapshot.files.len() >= MAX_FILES {
            snapshot.skipped += 1;
            continue;
        }
        let text = if exists {
            let Some(text) = shareable_text(work_tree, &path) else {
                snapshot.skipped += 1;
                continue;
            };
            if bytes + text.len() as u64 > MAX_SNAPSHOT_BYTES {
                snapshot.skipped += 1;
                continue;
            }
            bytes += text.len() as u64;
            Some(text)
        } else {
            None
        };
        snapshot.files.push(SharedFile { path, text });
    }
    snapshot
}

/// Scan the checkout the way `oak status` does, without storing anything.
/// Returns the head the changes are against, when the branch has one.
pub(crate) fn scan(oak_dir: &Path, work_tree: &Path) -> Result<Option<(Hash, Snapshot)>> {
    let repo = SqliteRepository::open(&oak_dir.join("oak.db"))?;
    let repo: &dyn Repository = &repo;
    let (changes, head, _) = crate::commands::commit::compute_changes_for_status(repo, work_tree)?;
    Ok(head.map(|head| (head, build_snapshot(work_tree, &changes))))
}

/// How viewers tell this checkout's work apart: the machine and the checkout
/// directory. The server keys snapshots on it, so two checkouts of one repo
/// don't replace each other's.
pub(crate) fn client_label(work_tree: &Path) -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            let output = std::process::Command::new("hostname").output().ok()?;
            String::from_utf8(output.stdout).ok()
        })
        .unwrap_or_default();
    let host = host.trim();
    let host = host.split('.').next().unwrap_or(host);
    let dir = work_tree
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label = match (host.is_empty(), dir.is_empty()) {
        (false, false) => format!("{host}/{dir}"),
        (false, true) => host.to_string(),
        (true, false) => dir,
        (true, true) => "oak-watch".to_string(),
    };
    label.chars().take(MAX_CLIENT_CHARS).collect()
}

/// `PUT .../working`. An empty `files` clears this client's snapshot.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn put_working(
    client: &reqwest::Client,
    remote: &str,
    repo_path: &str,
    branch: &str,
    api_key: Option<&str>,
    head: &str,
    label: &str,
    files: &[SharedFile],
) -> std::result::Result<reqwest::StatusCode, reqwest::Error> {
    let mut request = client
        .put(format!(
            "{remote}/api/{repo_path}/branches/{}/working",
            crate::commands::branch_api_segment(branch)
        ))
        .timeout(REQUEST_TIMEOUT)
        .json(&serde_json::json!({
            "head": head,
            "client": label,
            "files": files,
        }));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    Ok(request.send().await?.status())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, change_type: ChangeType, old_path: Option<&str>) -> FileChange {
        FileChange {
            path: path.to_string(),
            change_type,
            old_blob_hash: None,
            new_blob_hash: None,
            old_path: old_path.map(str::to_string),
            old_mode: None,
            new_mode: None,
        }
    }

    fn paths(snapshot: &Snapshot) -> Vec<&str> {
        snapshot.files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn credential_shaped_names_are_left_out() {
        for path in [
            ".env",
            "app/.env.production",
            "prod.env",
            "certs/server.pem",
            "deploy/id_ed25519",
            ".npmrc",
            "a/credentials.json",
            "tls.KEY",
        ] {
            assert!(looks_like_a_secret(path), "{path}");
        }
        for path in [
            ".env.example",
            "src/env.rs",
            "keyboard.rs",
            "docs/credentials.md",
            "environment.yml",
        ] {
            assert!(!looks_like_a_secret(path), "{path}");
        }
    }

    #[test]
    fn snapshot_covers_each_change_type_and_skips_what_it_cannot_share() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "added\n").unwrap();
        std::fs::write(root.join("new.txt"), "renamed\n").unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2]).unwrap();
        std::fs::write(root.join("latin1.txt"), [0xE9u8]).unwrap();
        std::fs::write(root.join("big.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        std::fs::write(root.join(".env"), "TOKEN=1\n").unwrap();

        let snapshot = build_snapshot(
            root,
            &[
                change("new.txt", ChangeType::Renamed, Some("old.txt")),
                change("a.txt", ChangeType::Added, None),
                change("gone.txt", ChangeType::Deleted, None),
                change("bin.dat", ChangeType::Modified, None),
                change("latin1.txt", ChangeType::Modified, None),
                change("big.txt", ChangeType::Modified, None),
                change(".env", ChangeType::Added, None),
                change("vanished.txt", ChangeType::Modified, None),
            ],
        );
        assert_eq!(
            paths(&snapshot),
            ["a.txt", "gone.txt", "new.txt", "old.txt"]
        );
        assert_eq!(snapshot.skipped, 5);
        let text = |path: &str| {
            snapshot
                .files
                .iter()
                .find(|file| file.path == path)
                .and_then(|file| file.text.clone())
        };
        assert_eq!(text("a.txt").as_deref(), Some("added\n"));
        assert_eq!(text("gone.txt"), None);
        assert_eq!(text("old.txt"), None, "a rename deletes the old path");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside the checkout").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let snapshot = build_snapshot(dir.path(), &[change("link", ChangeType::Added, None)]);
        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.skipped, 1);
    }

    #[test]
    fn snapshots_stay_within_the_server_limits() {
        let dir = tempfile::tempdir().unwrap();
        let mut changes = Vec::new();
        for i in 0..(MAX_FILES + 5) {
            let name = format!("f{i:04}.txt");
            std::fs::write(dir.path().join(&name), "x").unwrap();
            changes.push(change(&name, ChangeType::Added, None));
        }
        let snapshot = build_snapshot(dir.path(), &changes);
        assert_eq!(snapshot.files.len(), MAX_FILES);
        assert_eq!(snapshot.skipped, 5);

        let dir = tempfile::tempdir().unwrap();
        let mut changes = Vec::new();
        for i in 0..9 {
            let name = format!("f{i}.txt");
            std::fs::write(dir.path().join(&name), vec![b'x'; MAX_FILE_BYTES as usize]).unwrap();
            changes.push(change(&name, ChangeType::Added, None));
        }
        let snapshot = build_snapshot(dir.path(), &changes);
        assert_eq!(snapshot.files.len(), 8);
        assert_eq!(snapshot.skipped, 1);
    }

    #[test]
    fn fingerprints_follow_content_and_head() {
        let snapshot = |text: &str| Snapshot {
            files: vec![SharedFile {
                path: "a".to_string(),
                text: Some(text.to_string()),
            }],
            skipped: 0,
        };
        assert_eq!(
            snapshot("1").fingerprint("h"),
            snapshot("1").fingerprint("h")
        );
        assert_ne!(
            snapshot("1").fingerprint("h"),
            snapshot("2").fingerprint("h")
        );
        assert_ne!(
            snapshot("1").fingerprint("h"),
            snapshot("1").fingerprint("g")
        );
    }

    #[test]
    fn client_label_names_the_checkout_within_the_limit() {
        let label = client_label(Path::new("/tmp/my-checkout"));
        assert!(label.ends_with("my-checkout"), "{label}");
        let long = format!("/tmp/{}", "x".repeat(200));
        assert!(client_label(Path::new(&long)).chars().count() <= MAX_CLIENT_CHARS);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_sends_the_contract_body() {
        use wiremock::matchers::{body_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        crate::http::ensure_crypto_provider();
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/acme/demo/branches/a%2Fb/working"))
            .and(header("authorization", "Bearer key"))
            .and(body_json(serde_json::json!({
                "head": "abc",
                "client": "laptop/demo",
                "files": [
                    {"path": "a.rs", "text": "fn a() {}\n"},
                    {"path": "b.rs", "text": null},
                ],
            })))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let files = [
            SharedFile {
                path: "a.rs".to_string(),
                text: Some("fn a() {}\n".to_string()),
            },
            SharedFile {
                path: "b.rs".to_string(),
                text: None,
            },
        ];
        let status = put_working(
            &crate::http::api_client(),
            &server.uri(),
            "acme/demo",
            "a/b",
            Some("key"),
            "abc",
            "laptop/demo",
            &files,
        )
        .await
        .unwrap();
        assert!(status.is_success());
    }
}
