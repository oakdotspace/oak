//! `oak finish --json` may only report `description_synced: true` after a
//! confirmed metadata publication of the description it staged.
//!
//! Regression: a clean branch whose commits were already pushed but not yet
//! merged has a non-zero unmerged-commit count, so finish took its push leg.
//! That push was a no-op ("Already up to date", no branch row sent), yet
//! finish reported `description_synced: true` while the server kept the old
//! description and the local row stayed `description_pending`.
//!
//! Runs against a real loopback `oak serve`.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use oak_core::{Repository, SqliteRepository};

const SERVE_TOKEN: &str = "finish-serve-token";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn oak(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(cwd)
        .env("HOME", home)
        .env("OAK_API_KEY", SERVE_TOKEN)
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_REPO")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .args(args)
        .output()
        .unwrap()
}

fn ok(output: Output, label: &str) -> Output {
    assert!(
        output.status.success(),
        "{label} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn start_serve(dir: &Path) -> (ChildGuard, String) {
    oak_cli::http::ensure_crypto_provider();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--token",
            SERVE_TOKEN,
        ])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard = ChildGuard(child);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..200 {
        if client
            .get(format!("{base}/api/capabilities"))
            .send()
            .await
            .is_ok()
        {
            return (guard, base);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("oak serve did not become ready");
}

async fn server_description(base: &str, repo: &str, branch: &str) -> Option<String> {
    let response: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/oak/{repo}/pull"))
        .query(&[("branch_name", branch)])
        .bearer_auth(SERVE_TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    response["branch"]["description"]
        .as_str()
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_after_noop_push_publishes_and_confirms_description() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let checkout = temp.path().join("checkout");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    let (_serve, base) = start_serve(&temp.path().join("serve-data")).await;

    ok(oak(&home, &checkout, &["init", "."]), "init");
    std::fs::write(checkout.join("file.txt"), "content\n").unwrap();
    ok(oak(&home, &checkout, &["commit"]), "commit");
    ok(
        oak(&home, &checkout, &["desc", "first description"]),
        "desc",
    );
    ok(
        oak(
            &home,
            &checkout,
            &["push", "--repo", "oak/finish-sync", "-r", &base, "--json"],
        ),
        "push",
    );
    let branch = SqliteRepository::open(&checkout.join(".oak/oak.db"))
        .unwrap()
        .get_current_branch_name()
        .unwrap()
        .unwrap();
    assert_eq!(
        server_description(&base, "finish-sync", &branch)
            .await
            .as_deref(),
        Some("first description")
    );

    // Clean tree, commits already on the server but not merged: the push
    // leg runs and publishes nothing.
    let output = ok(
        oak(
            &home,
            &checkout,
            &["finish", "--desc", "final description", "--json"],
        ),
        "finish",
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["phase"], "complete");
    assert_eq!(json["description_synced"], true);
    assert!(
        json["completed_phases"]
            .as_array()
            .unwrap()
            .iter()
            .any(|phase| phase == "metadata_sync"),
        "{json}"
    );

    // The claim is backed by the server and by local sync tracking.
    assert_eq!(
        server_description(&base, "finish-sync", &branch)
            .await
            .as_deref(),
        Some("final description")
    );
    let repo = SqliteRepository::open(&checkout.join(".oak/oak.db")).unwrap();
    assert!(!repo.branch_description_pending(&branch).unwrap());
}
