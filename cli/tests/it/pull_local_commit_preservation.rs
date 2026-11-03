//! `oak pull` must never silently discard local-only commits.
//!
//! Regression (Blocker, data loss): against loopback `oak serve`, push a
//! branch, commit locally, then plain `oak pull` exited 0, reset the branch to
//! the server head, deleted the committed file from the worktree, and parked
//! nothing. Serve answered the unknown `?since=` with 200 + the branch's full
//! history (hosted oakspace answers 409), and the client adopted the response
//! head without checking that it descends from the local tip.
//!
//! Runs the real binary against a real loopback `oak serve`.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use oak_core::{Hash, Repository, SqliteRepository};

const SERVE_TOKEN: &str = "pull-preservation-token";

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
        .env_remove("OAK_TOKEN")
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

/// Start `oak serve` on an OS-assigned port (`--port 0`, no bind-and-drop
/// race) and read the bound address from its startup line. On failure the
/// panic carries serve's own output.
async fn start_serve(dir: &Path) -> (ChildGuard, String) {
    use std::io::{BufRead, BufReader};
    oak_cli::http::ensure_crypto_provider();
    // Serve's stderr goes to a file (never an undrained pipe) and is quoted
    // in any readiness panic.
    let stderr_path = dir.with_extension("serve-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oak"))
        .args([
            "serve",
            "--dir",
            dir.to_str().unwrap(),
            "--port",
            "0",
            "--token",
            SERVE_TOKEN,
        ])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        let mut seen = String::new();
        for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
            if let Some(rest) = line.strip_prefix("listening on ") {
                let url = rest.split_whitespace().next().unwrap_or("").to_string();
                let _ = tx.send(Ok(url));
                return;
            }
            seen.push_str(&line);
            seen.push('\n');
        }
        let _ = tx.send(Err(seen));
    });
    let guard = ChildGuard(child);
    let base = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(base)) => base,
        other => {
            drop(guard);
            let err = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!("oak serve did not report a listening address: {other:?}; stderr={err}");
        }
    };
    let client = reqwest::Client::new();
    for _ in 0..400 {
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
    panic!(
        "oak serve at {base} did not answer; stderr={}",
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );
}

fn open(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

fn current(dir: &Path) -> (String, Hash) {
    let repo = open(dir);
    let branch = repo.get_current_branch_name().unwrap().unwrap();
    let head = repo.get_branch_head(&branch).unwrap().unwrap();
    (branch, head)
}

struct Fixture {
    _temp: tempfile::TempDir,
    _serve: ChildGuard,
    base: String,
    home: std::path::PathBuf,
    root: std::path::PathBuf,
    a: std::path::PathBuf,
}

/// Checkout `a` on a fresh branch with one commit pushed to `l9/r`.
async fn pushed_fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("home");
    let a = root.join("a");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&a).unwrap();
    let (serve, base) = start_serve(&root.join("serve-data")).await;
    ok(oak(&home, &a, &["init", "."]), "init");
    std::fs::write(a.join("one.txt"), "one\n").unwrap();
    ok(oak(&home, &a, &["commit"]), "commit one");
    ok(
        oak(&home, &a, &["push", "--repo", "l9/r", "-r", &base]),
        "push",
    );
    Fixture {
        _temp: temp,
        _serve: serve,
        base,
        home,
        root,
        a,
    }
}

fn commit_local(home: &Path, dir: &Path, file: &str) -> Hash {
    std::fs::write(dir.join(file), format!("{file}\n")).unwrap();
    ok(oak(home, dir, &["commit"]), "local commit");
    current(dir).1
}

fn assert_no_orphan(dir: &Path) {
    let orphans: Vec<String> = open(dir)
        .list_branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .filter(|name| name.contains(".orphaned-"))
        .collect();
    assert!(
        orphans.is_empty(),
        "unexpected parked branches: {orphans:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_pull_keeps_unpushed_commit_on_pushed_branch() {
    let f = pushed_fixture().await;
    let local_tip = commit_local(&f.home, &f.a, "local.txt");

    let out = ok(oak(&f.home, &f.a, &["pull"]), "pull");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let (_, head) = current(&f.a);
    assert_eq!(
        head, local_tip,
        "pull must not move the branch off an unpushed commit; stdout={stdout} stderr={stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(f.a.join("local.txt")).unwrap(),
        "local.txt\n",
        "the committed file must stay in the worktree"
    );
    assert!(
        format!("{stdout}{stderr}").contains("local commits not pushed yet"),
        "pull must say the local commits were kept: stdout={stdout} stderr={stderr}"
    );
    assert_no_orphan(&f.a);

    // Still publishable: the next push extends the server head.
    ok(oak(&f.home, &f.a, &["push"]), "push after pull");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_pull_on_shallow_clone_keeps_unpushed_commit() {
    let f = pushed_fixture().await;
    let (branch, _) = current(&f.a);
    let b = f.root.join("b");
    ok(
        oak(
            &f.home,
            &f.root,
            &[
                "clone",
                "l9/r",
                b.to_str().unwrap(),
                "-r",
                &f.base,
                "--branch",
                &branch,
                "--shallow",
                "--allow-legacy-scope",
            ],
        ),
        "shallow clone",
    );
    commit_local(&f.home, &b, "b1.txt");
    ok(oak(&f.home, &b, &["push"]), "push from shallow clone");
    let local_tip = commit_local(&f.home, &b, "local.txt");

    ok(oak(&f.home, &b, &["pull"]), "pull");

    assert_eq!(current(&b).1, local_tip);
    assert!(b.join("local.txt").exists(), "local file must survive");
    assert!(b.join("b1.txt").exists());
    assert_no_orphan(&b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_pull_converges_diverged_branch_and_keeps_local_commit() {
    let f = pushed_fixture().await;
    let (branch, _) = current(&f.a);

    // Another checkout advances the same branch on the server.
    let c = f.root.join("c");
    ok(
        oak(
            &f.home,
            &f.root,
            &[
                "clone",
                "l9/r",
                c.to_str().unwrap(),
                "-r",
                &f.base,
                "--branch",
                &branch,
            ],
        ),
        "clone c",
    );
    commit_local(&f.home, &c, "other.txt");
    ok(oak(&f.home, &c, &["push"]), "push from c");
    let remote_tip = current(&c).1;

    let local_tip = commit_local(&f.home, &f.a, "local.txt");
    ok(oak(&f.home, &f.a, &["pull"]), "pull");

    let repo = open(&f.a);
    let (_, head) = current(&f.a);
    assert_ne!(head, remote_tip, "must not reset onto the remote tip");
    let converged = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(converged.parent_hash.as_ref(), Some(&remote_tip));
    assert_eq!(
        converged.merge_parent_hash.as_ref(),
        Some(&local_tip),
        "old local tip must stay reachable"
    );
    assert!(f.a.join("local.txt").exists(), "local work kept");
    assert!(f.a.join("other.txt").exists(), "remote work brought in");
    assert_no_orphan(&f.a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_pull_parks_the_local_tip_before_replacing_it() {
    let f = pushed_fixture().await;
    let (branch, remote_tip) = current(&f.a);
    let local_tip = commit_local(&f.home, &f.a, "local.txt");

    let out = ok(oak(&f.home, &f.a, &["pull", "--force"]), "pull --force");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let repo = open(&f.a);
    assert_eq!(current(&f.a).1, remote_tip, "--force adopts the remote");
    let parked = repo
        .list_branches()
        .unwrap()
        .into_iter()
        .find(|b| b.name.starts_with(&format!("{branch}.orphaned-")))
        .expect("--force must park the old tip");
    assert_eq!(repo.get_branch_head(&parked.name).unwrap(), Some(local_tip));
    assert!(
        text.contains(&format!("Parked local commits as '{}'", parked.name)),
        "must name the parked ref: {text}"
    );
}

/// Serve mirrors hosted oakspace: an unknown `since` (local-only commits)
/// is a 409, not 200 + full history — so released clients, which trust the
/// response head, converge instead of discarding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_answers_unknown_or_unreachable_since_with_conflict() {
    let f = pushed_fixture().await;
    let (branch, pushed) = current(&f.a);
    let client = reqwest::Client::new();
    let pull = |params: Vec<(&'static str, String)>| {
        client
            .get(format!("{}/api/l9/r/pull", f.base))
            .query(&params)
            .bearer_auth(SERVE_TOKEN)
            .send()
    };
    let unknown = "ab".repeat(32);

    let resp = pull(vec![
        ("since", unknown.clone()),
        ("branch_name", branch.clone()),
    ])
    .await
    .unwrap();
    assert_eq!(resp.status().as_u16(), 409);

    let resp = pull(vec![
        ("since", unknown),
        ("branch_name", branch.clone()),
        ("force", "true".to_string()),
    ])
    .await
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "force skips the check");

    let resp = pull(vec![
        ("since", pushed.to_string()),
        ("branch_name", branch.clone()),
    ])
    .await
    .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "known head: ordinary pull");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["commits"].as_array().unwrap().len(), 0);
}

/// QA-L9-A F2: a diverged pull over uncommitted edits must refuse (exit 4)
/// before moving anything. Converging would leave the remote side's changes
/// unmaterialized, and the next `oak commit` would silently revert them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diverged_pull_with_uncommitted_edits_refuses_without_moving() {
    let f = pushed_fixture().await;
    let (branch, _) = current(&f.a);
    let c = f.root.join("c");
    ok(
        oak(
            &f.home,
            &f.root,
            &[
                "clone",
                "l9/r",
                c.to_str().unwrap(),
                "-r",
                &f.base,
                "--branch",
                &branch,
            ],
        ),
        "clone c",
    );
    std::fs::write(c.join("one.txt"), "remote edit\n").unwrap();
    commit_local(&f.home, &c, "remote.txt");
    ok(oak(&f.home, &c, &["push"]), "push from c");
    let remote_tip = current(&c).1;

    let local_tip = commit_local(&f.home, &f.a, "local.txt");
    std::fs::write(f.a.join("wip.txt"), "wip\n").unwrap();

    let out = oak(&f.home, &f.a, &["pull"]);
    assert_eq!(
        out.status.code(),
        Some(4),
        "dirty diverged pull must refuse: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(current(&f.a).1, local_tip, "nothing moved");
    assert_eq!(
        std::fs::read_to_string(f.a.join("wip.txt")).unwrap(),
        "wip\n"
    );
    assert_eq!(
        std::fs::read_to_string(f.a.join("one.txt")).unwrap(),
        "one\n"
    );
    assert!(!f.a.join(".oak/SYNC_HEAD").exists());

    // After committing the WIP, the pull converges and the remote side's
    // content is on disk and in the converged snapshot — nothing reverted.
    ok(oak(&f.home, &f.a, &["commit"]), "commit wip");
    ok(oak(&f.home, &f.a, &["pull"]), "pull after commit");
    let repo = open(&f.a);
    let head = current(&f.a).1;
    let converged = repo.get_commit(&head).unwrap().unwrap();
    assert_eq!(converged.parent_hash.as_ref(), Some(&remote_tip));
    assert_eq!(
        std::fs::read_to_string(f.a.join("one.txt")).unwrap(),
        "remote edit\n"
    );
    assert!(f.a.join("remote.txt").exists() && f.a.join("wip.txt").exists());
    let status = ok(oak(&f.home, &f.a, &["status", "--json"]), "status");
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        json["working_changes"]["changes"].as_array().unwrap().len(),
        0,
        "{json}"
    );
}

/// QA-L9-A round 2 (reviewer test): a pull from a DETACHED HEAD must not move
/// a named local branch whose head holds unpushed commits. The response head
/// is labelled with that branch (serve's default branch here), but the pull
/// did not select it, so its local head must be containment-checked like any
/// other non-selected branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qa_detached_pull_keeps_unpushed_commit_on_named_branch() {
    let f = pushed_fixture().await;
    let (branch, pushed) = current(&f.a);
    let local_tip = commit_local(&f.home, &f.a, "local.txt");
    ok(
        oak(&f.home, &f.a, &["switch", "-d", pushed.as_str()]),
        "detach",
    );
    let out = oak(&f.home, &f.a, &["pull"]);
    let head = open(&f.a).get_branch_head(&branch).unwrap();
    assert_eq!(
        head,
        Some(local_tip),
        "detached pull moved '{branch}' off its unpushed commit: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// R2-1 companion: `--force` from a detached HEAD may adopt the remote for
/// the branch the response is about, but only after parking its local tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_force_pull_parks_named_branch_before_moving_it() {
    let f = pushed_fixture().await;
    let (branch, pushed) = current(&f.a);
    let local_tip = commit_local(&f.home, &f.a, "local.txt");
    ok(
        oak(&f.home, &f.a, &["switch", "-d", pushed.as_str()]),
        "detach",
    );
    let out = oak(&f.home, &f.a, &["pull", "--force"]);
    let repo = open(&f.a);
    let head = repo.get_branch_head(&branch).unwrap();
    if head == Some(local_tip.clone()) {
        // Kept in place: nothing to park.
        return;
    }
    let parked = repo
        .list_branches()
        .unwrap()
        .into_iter()
        .find(|b| b.name.starts_with(&format!("{branch}.orphaned-")))
        .unwrap_or_else(|| {
            panic!(
                "moved '{branch}' without parking: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
    assert_eq!(repo.get_branch_head(&parked.name).unwrap(), Some(local_tip));
}

fn clone_branch(f: &Fixture, branch: &str, dir: &str) -> std::path::PathBuf {
    let path = f.root.join(dir);
    ok(
        oak(
            &f.home,
            &f.root,
            &[
                "clone",
                "l9/r",
                path.to_str().unwrap(),
                "-r",
                &f.base,
                "--branch",
                branch,
            ],
        ),
        "clone",
    );
    path
}

/// QA-L9-B F3: the server's `since` walk is first-parent only, so a clean
/// checkout whose head is now reachable only through a MERGE parent gets a
/// 409. The client must prove containment (verified rows) and fast-forward —
/// not mint a spurious empty re-parent commit that leaves it "ahead".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_pull_fast_forwards_when_tip_is_only_a_merge_parent_ancestor() {
    let f = pushed_fixture().await;
    let (branch, _h1) = current(&f.a);
    let b = clone_branch(&f, &branch, "b");
    let c = clone_branch(&f, &branch, "c");

    commit_local(&f.home, &f.a, "h2.txt");
    ok(oak(&f.home, &f.a, &["push"]), "push h2");
    ok(oak(&f.home, &c, &["pull"]), "c pulls h2");
    let c_tip = current(&c).1;

    commit_local(&f.home, &b, "rw.txt");
    ok(oak(&f.home, &b, &["push", "-f"]), "b force-pushes rw");

    commit_local(&f.home, &f.a, "a3.txt");
    ok(oak(&f.home, &f.a, &["pull"]), "a re-parents onto rw");
    ok(oak(&f.home, &f.a, &["push"]), "a pushes r");
    let r = current(&f.a).1;

    let out = ok(oak(&f.home, &c, &["pull", "--json"]), "c pulls");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(current(&c).1, r, "c must fast-forward to r: {json}");
    assert_eq!(json["local_commits"], "contained_by_remote", "{json}");
    assert_eq!(json["branch_update"], "fast_forward", "{json}");
    let repo = open(&c);
    assert!(repo.get_commit(&c_tip).unwrap().is_some());
    for file in ["h2.txt", "rw.txt", "a3.txt"] {
        assert!(c.join(file).exists(), "{file} materialized");
    }
    let status = ok(oak(&f.home, &c, &["status", "--json"]), "status");
    let st: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        st["working_changes"]["changes"].as_array().unwrap().len(),
        0
    );
}

/// QA-L9-B F1: loopback serve implements hosted's `POST /blobs/info`, so
/// bounded hydration (re-parent seeds, mounts) never needs a whole-branch
/// pull. Published blobs come back; unknown hashes are omitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_blob_info_returns_published_blobs_only() {
    let f = pushed_fixture().await;
    let repo = open(&f.a);
    let head = current(&f.a).1;
    let commit = repo.get_commit(&head).unwrap().unwrap();
    let manifest = repo.get_manifest(&commit.manifest_hash).unwrap().unwrap();
    let blob = manifest.entries[0].blob_hash.to_string();
    let unknown = "ef".repeat(32);
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/api/l9/r/blobs/info", f.base))
        .bearer_auth(SERVE_TOKEN)
        .json(&serde_json::json!({ "hashes": [blob, unknown] }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let blobs = resp["blobs"].as_array().unwrap();
    assert_eq!(blobs.len(), 1, "{resp}");
    assert_eq!(blobs[0]["hash"], blob.as_str());
    assert_eq!(blobs[0]["size"], 4);
}
