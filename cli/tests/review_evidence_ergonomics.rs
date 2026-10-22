//! W4a review/diff evidence ergonomics:
//! fb-334 structured line numbers, fb-453 evidence_kind + per-row basis,
//! fb-470 ancestry_diagnostic, fb-410 `oak diff --remote OLD NEW`.
use oak_cli::commands::review::{DiffJsonOptions, DiffMode};
use oak_core::{
    Branch, Commit, FileMode, Hash, Manifest, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BASE_A: &[u8] = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
const FEATURE_A: &[u8] =
    b"one\ntwo\nTHREE\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\n";
const MAIN_ONLY: &[u8] = b"added on main after the fork\n";

fn ts() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn manifest(repo: &SqliteRepository, files: &[(&str, &[u8])]) -> Manifest {
    let manifest = Manifest::new(
        files
            .iter()
            .map(|(path, bytes)| ManifestEntry {
                path: (*path).into(),
                blob_hash: repo.put_blob(bytes.to_vec()).unwrap(),
                mode: FileMode::Regular,
            })
            .collect(),
    );
    repo.store_manifest(&manifest).unwrap();
    manifest
}

fn commit(
    repo: &SqliteRepository,
    branch: &str,
    parent: Option<Hash>,
    manifest: &Manifest,
    author: &str,
) -> Commit {
    let commit = Commit::with_timestamp(
        branch.into(),
        parent,
        None,
        manifest.hash.clone(),
        author.into(),
        None,
        vec![],
        ts(),
    )
    .unwrap();
    repo.store_commit(&commit).unwrap();
    commit
}

struct Local {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: SqliteRepository,
    base: Commit,
    shallow_parent: Hash,
}

/// main: M0 (a.txt) -> M1 (+ main_only.txt); feature: M0 -> F1 (edits a.txt).
/// M0's own parent is not stored (a shallow boundary below the fork).
fn local() -> Local {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir(root.join(".oak")).unwrap();
    let repo = SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();
    let shallow_parent = oak_core::hash_bytes(b"never stored parent");
    let m0 = manifest(&repo, &[("a.txt", BASE_A)]);
    let base = commit(&repo, "main", Some(shallow_parent.clone()), &m0, "m0");
    let m1 = manifest(&repo, &[("a.txt", BASE_A), ("main_only.txt", MAIN_ONLY)]);
    let main = commit(&repo, "main", Some(base.hash.clone()), &m1, "m1");
    let f1 = manifest(&repo, &[("a.txt", FEATURE_A)]);
    let feature = commit(&repo, "feature", Some(base.hash.clone()), &f1, "f1");
    repo.store_branch(&Branch::new("main".into(), None, None))
        .unwrap();
    repo.store_branch(&Branch::new("feature".into(), None, Some("main".into())))
        .unwrap();
    repo.set_branch_head("main", &main.hash).unwrap();
    repo.set_branch_head("feature", &feature.hash).unwrap();
    repo.set_current_branch("main").unwrap();
    Local {
        _temp: temp,
        root,
        repo,
        base,
        shallow_parent,
    }
}

fn capture<T>(run: impl FnOnce() -> oak_core::Result<T>) -> serde_json::Value {
    oak_cli::output::begin_capture();
    let result = run();
    let captured = oak_cli::output::end_capture();
    result.unwrap();
    serde_json::from_str(captured.trim()).unwrap()
}

fn branch_diff(fx: &Local, mode: DiffMode, options: DiffJsonOptions) -> serde_json::Value {
    capture(|| {
        oak_cli::commands::review::branch_diff_json(&fx.root, "feature", "main", mode, &[], options)
    })
}

fn file<'a>(json: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    json["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == path)
        .unwrap_or_else(|| panic!("{path} missing: {json}"))
}

fn line_numbers() -> DiffJsonOptions {
    DiffJsonOptions {
        hunks: true,
        line_numbers: true,
        ..Default::default()
    }
}

#[test]
fn fb334_structured_hunks_cite_post_image_lines() {
    let fx = local();
    let json = branch_diff(&fx, DiffMode::Contribution, line_numbers());
    let a = file(&json, "a.txt");
    assert!(a["patch"].is_string());
    let post: Vec<&str> = std::str::from_utf8(FEATURE_A).unwrap().lines().collect();
    let pre: Vec<&str> = std::str::from_utf8(BASE_A).unwrap().lines().collect();
    let mut added = Vec::new();
    for hunk in a["hunks"].as_array().unwrap() {
        for key in ["old_start", "old_len", "new_start", "new_len"] {
            assert!(hunk[key].is_u64(), "{hunk}");
        }
        for line in hunk["lines"].as_array().unwrap() {
            let text = line["text"].as_str().unwrap();
            if let Some(n) = line["new_line"].as_u64() {
                assert_eq!(post[n as usize - 1], text, "{line}");
            }
            if let Some(n) = line["old_line"].as_u64() {
                assert_eq!(pre[n as usize - 1], text, "{line}");
            }
            if line["kind"] == "added" {
                assert!(line["old_line"].is_null());
                added.push(line["new_line"].as_u64().unwrap());
            }
        }
    }
    assert_eq!(added, vec![3, 11, 12]);

    // Opt-in: without --line-numbers the payload is unchanged.
    let plain = branch_diff(
        &fx,
        DiffMode::Contribution,
        DiffJsonOptions {
            hunks: true,
            ..Default::default()
        },
    );
    assert!(file(&plain, "a.txt").get("hunks").is_none());

    // Bounded like patches: a file whose patch is budget-omitted has no hunks.
    let budget = branch_diff(
        &fx,
        DiffMode::Tree,
        DiffJsonOptions {
            max_bytes: Some(1),
            ..line_numbers()
        },
    );
    for row in budget["changed_files"].as_array().unwrap() {
        assert_eq!(row["patch_omitted"], true, "{row}");
        assert!(row.get("hunks").is_none(), "{row}");
    }
}

#[test]
fn fb453_snapshot_rows_are_qualified_and_contribution_rows_are_not() {
    let fx = local();
    let tree = branch_diff(&fx, DiffMode::Tree, Default::default());
    assert_eq!(tree["evidence_kind"], "snapshot");
    let main_only = file(&tree, "main_only.txt");
    assert_eq!(main_only["status"], "deleted");
    assert_eq!(main_only["basis"], "snapshot_absence");
    assert_eq!(file(&tree, "a.txt")["basis"], "snapshot_difference");

    let contribution = branch_diff(&fx, DiffMode::Contribution, Default::default());
    assert_eq!(contribution["evidence_kind"], "contribution");
    for row in contribution["changed_files"].as_array().unwrap() {
        assert!(row.get("basis").is_none(), "{row}");
        assert_ne!(row["path"], "main_only.txt");
    }

    let net = branch_diff(&fx, DiffMode::NetMerge, Default::default());
    assert_eq!(net["evidence_kind"], "predicted_merge");
    assert!(net["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row.get("basis").is_none()));

    let review = capture(|| {
        oak_cli::commands::review::branch_review_json(
            &fx.root, "feature", true, "main", None, 0, None,
        )
    });
    assert_eq!(review["evidence_kind"], "snapshot");
    assert_eq!(file(&review, "main_only.txt")["basis"], "snapshot_absence");
    assert_eq!(review["merge_preview"]["evidence_kind"], "predicted_merge");
}

#[test]
fn fb470_residual_gap_below_the_certified_base_is_reported_irrelevant() {
    let fx = local();
    for json in [
        branch_diff(&fx, DiffMode::Tree, Default::default()),
        capture(|| {
            oak_cli::commands::review::branch_review_json(
                &fx.root, "feature", false, "main", None, 0, None,
            )
        }),
    ] {
        let diag = &json["ancestry_diagnostic"];
        assert_eq!(diag["resolution"], "certified", "{json}");
        assert_eq!(diag["proven_base"], fx.base.hash.to_string());
        assert_eq!(
            diag["missing"],
            serde_json::json!([fx.shallow_parent.to_string()])
        );
        assert_eq!(diag["below_proven_base"], true);
    }
}

#[test]
fn fb470_blocking_gap_keeps_merge_base_relevance_unknown() {
    let fx = local();
    // A branch whose only commit's parent was never acquired: nothing links
    // it to main, and the gap could hide a nearer base.
    let gap_parent = oak_core::hash_bytes(b"unacquired branch parent");
    let m = manifest(&fx.repo, &[("a.txt", FEATURE_A)]);
    let head = commit(&fx.repo, "gap", Some(gap_parent.clone()), &m, "g1");
    fx.repo
        .store_branch(&Branch::new("gap".into(), None, Some("main".into())))
        .unwrap();
    fx.repo.set_branch_head("gap", &head.hash).unwrap();
    let json = capture(|| {
        oak_cli::commands::review::branch_diff_json(
            &fx.root,
            "gap",
            "main",
            DiffMode::Tree,
            &[],
            Default::default(),
        )
    });
    let diag = &json["ancestry_diagnostic"];
    assert_eq!(diag["resolution"], "incomplete_ancestry", "{json}");
    assert_eq!(diag["below_proven_base"], "unknown");
    assert!(diag.get("proven_base").is_none());
    let missing: Vec<&str> = diag["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    assert!(missing.contains(&gap_parent.as_str()), "{diag}");
    assert_eq!(diag["missing_count"], missing.len());
}

// ---------------------------------------------------------------------------
// fb-410: `oak diff --remote OLD NEW`
// ---------------------------------------------------------------------------

struct Remote {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: SqliteRepository,
    server: MockServer,
    local_main: Hash,
    old: Commit,
    new: Commit,
    old_manifest: Manifest,
    new_manifest: Manifest,
}

fn remote_manifest(files: &[(&str, &[u8])]) -> Manifest {
    Manifest::new(
        files
            .iter()
            .map(|(path, bytes)| ManifestEntry {
                path: (*path).into(),
                blob_hash: oak_core::hash_bytes(bytes),
                mode: FileMode::Regular,
            })
            .collect(),
    )
}

fn wire(commit: &Commit) -> oak_core::protocol::CommitData {
    oak_core::protocol::CommitData {
        hash: commit.hash.to_string(),
        branch_name: commit.branch_name.clone(),
        parent_hash: commit.parent_hash.as_ref().map(ToString::to_string),
        merge_parent_hash: None,
        manifest_hash: commit.manifest_hash.to_string(),
        author: commit.author.clone(),
        message: None,
        timestamp: commit.timestamp.to_rfc3339(),
        files: vec![],
    }
}

/// A checkout that knows neither remote commit: local main is unrelated.
async fn remote() -> Remote {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir(root.join(".oak")).unwrap();
    let repo = SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();
    let local_manifest = manifest(&repo, &[("local.txt", b"local\n")]);
    let local_main = commit(&repo, "main", None, &local_manifest, "local");
    repo.store_branch(&Branch::new("main".into(), Some("desc".into()), None))
        .unwrap();
    repo.set_branch_head("main", &local_main.hash).unwrap();
    repo.set_current_branch("main").unwrap();

    let old_manifest = remote_manifest(&[("a.txt", BASE_A), ("gone.txt", b"bye\n")]);
    let new_manifest = remote_manifest(&[("a.txt", FEATURE_A), ("new.txt", b"hi\n")]);
    let old = Commit::with_timestamp(
        "main".into(),
        None,
        None,
        old_manifest.hash.clone(),
        "r0".into(),
        None,
        vec![],
        ts(),
    )
    .unwrap();
    let new = Commit::with_timestamp(
        "main".into(),
        Some(old.hash.clone()),
        None,
        new_manifest.hash.clone(),
        "r1".into(),
        None,
        vec![],
        ts(),
    )
    .unwrap();

    let server = MockServer::start().await;
    for (key, value) in [
        (MetadataKey::RemoteUrl, server.uri()),
        (MetadataKey::RepoOwner, "oak".into()),
        (MetadataKey::RepoName, "oak".into()),
    ] {
        repo.set_metadata(key, &value).unwrap();
    }
    let mut trees = Vec::new();
    for manifest in [&old_manifest, &new_manifest] {
        for tree in oak_core::build_tree(&manifest.entries).unwrap().trees {
            trees.push(oak_core::protocol::tree_to_wire(&tree));
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"commits":[wire(&old), wire(&new)],"trees":trees}),
            ),
        )
        .mount(&server)
        .await;
    for (head, files) in [
        (
            &old.hash,
            vec![("a.txt", BASE_A), ("gone.txt", b"bye\n".as_slice())],
        ),
        (
            &new.hash,
            vec![("a.txt", FEATURE_A), ("new.txt", b"hi\n".as_slice())],
        ),
    ] {
        for (file, bytes) in files {
            Mock::given(method("GET"))
                .and(path(format!("/api/oak/oak/raw/{head}/{file}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
                .mount(&server)
                .await;
        }
    }
    Remote {
        _temp: temp,
        root,
        repo,
        server,
        local_main: local_main.hash,
        old,
        new,
        old_manifest,
        new_manifest,
    }
}

impl Remote {
    async fn run(
        &self,
        args: &[String],
        json: bool,
        options: DiffJsonOptions,
    ) -> oak_core::Result<(bool, String)> {
        let args: Vec<std::path::PathBuf> = args.iter().map(Into::into).collect();
        oak_cli::output::begin_capture();
        let result =
            oak_cli::commands::review::remote_endpoint_diff(&self.root, &args, None, json, options)
                .await;
        let captured = oak_cli::output::end_capture();
        result.map(|differs| (differs, captured))
    }

    fn endpoints(&self) -> Vec<String> {
        vec![self.old.hash.to_string(), self.new.hash.to_string()]
    }

    async fn requests(&self, needle: &str) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().contains(needle))
            .count()
    }

    fn assert_refs_unchanged(&self) {
        assert_eq!(
            self.repo.get_branch_head("main").unwrap(),
            Some(self.local_main.clone())
        );
        assert_eq!(
            self.repo.get_current_branch_name().unwrap().as_deref(),
            Some("main")
        );
        let branches: Vec<String> = self
            .repo
            .list_branches()
            .unwrap()
            .into_iter()
            .map(|branch| branch.name)
            .collect();
        assert_eq!(branches, vec!["main".to_string()]);
        assert_eq!(
            self.repo
                .get_branch("main")
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("desc")
        );
    }
}

#[tokio::test]
async fn fb410_remote_endpoint_diff_fetches_only_the_two_commits_and_moves_no_refs() {
    let fx = remote().await;
    assert!(fx.repo.get_commit(&fx.old.hash).unwrap().is_none());

    // Summary: metadata only (one commits/info request, no raw reads).
    let (differs, out) = fx
        .run(&fx.endpoints(), true, Default::default())
        .await
        .unwrap();
    assert!(differs);
    let summary: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(summary["kind"], "remote_endpoint_diff");
    assert_eq!(summary["evidence_kind"], "snapshot");
    assert_eq!(summary["against"], fx.old.hash.to_string());
    assert_eq!(summary["branch_head"], fx.new.hash.to_string());
    assert_eq!(summary["changed_file_count"], 3);
    assert_eq!(file(&summary, "gone.txt")["basis"], "snapshot_absence");
    assert_eq!(fx.requests("/commits/info").await, 1);
    assert_eq!(fx.requests("/raw/").await, 0);

    // Hunks: warm commits (no second commits/info), exact raw reads only.
    let (_, out) = fx.run(&fx.endpoints(), true, line_numbers()).await.unwrap();
    let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(fx.requests("/commits/info").await, 1, "commits are cached");
    assert_eq!(
        fx.requests("/raw/").await,
        4,
        "a.txt on both sides + gone.txt old + new.txt new"
    );
    assert_eq!(json["acquisition"]["commit_info_requests"], 0);
    let a = file(&json, "a.txt");
    assert!(a["patch"].as_str().unwrap().contains("+THREE"));
    let added: Vec<u64> = a["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|hunk| hunk["lines"].as_array().unwrap().clone())
        .filter(|line| line["kind"] == "added")
        .map(|line| line["new_line"].as_u64().unwrap())
        .collect();
    assert_eq!(added, vec![3, 11, 12]);
    // Verified bytes for both manifests are now cached.
    for manifest in [&fx.old_manifest, &fx.new_manifest] {
        for entry in &manifest.entries {
            assert!(fx.repo.has_blob(&entry.blob_hash).unwrap());
        }
    }

    // Print with gutters, path-filtered.
    let mut args = fx.endpoints();
    args.push("a.txt".into());
    let (_, printed) = fx
        .run(
            &args,
            false,
            DiffJsonOptions {
                hunks: true,
                line_numbers: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(printed.contains("diff --oak a/a.txt b/a.txt"), "{printed}");
    assert!(!printed.contains("new.txt"));
    assert!(
        printed.lines().any(|line| line == "            3 +THREE"),
        "{printed}"
    );
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn fb410_remote_endpoints_must_be_full_hashes_and_nothing_is_requested_otherwise() {
    let fx = remote().await;
    let prefix = fx.new.hash.as_str()[..12].to_string();
    for args in [
        vec![fx.old.hash.to_string(), prefix],
        vec![fx.old.hash.to_string(), "main".to_string()],
        vec![fx.old.hash.to_string()],
    ] {
        let error = fx.run(&args, true, Default::default()).await.unwrap_err();
        assert!(error.to_string().contains("full commit hash"), "{error}");
    }
    assert_eq!(fx.server.received_requests().await.unwrap().len(), 0);
    fx.assert_refs_unchanged();
}

// ---------------------------------------------------------------------------
// fb-432: `oak log --remote --json`
// ---------------------------------------------------------------------------

/// Remote main: M0 (root) <- M1 <- M2 (merge of feature@F1, message cites
/// feedback). None of them is in the local store.
async fn remote_log_fixture() -> (Remote, Vec<Commit>, Commit) {
    let fx = remote().await;
    let m = remote_manifest(&[("a.txt", BASE_A)]);
    let make = |branch: &str, parent: Option<Hash>, merge: Option<Hash>, message: Option<&str>| {
        Commit::with_timestamp(
            branch.into(),
            parent,
            merge,
            m.hash.clone(),
            "qa".into(),
            message.map(str::to_string),
            vec![],
            ts(),
        )
        .unwrap()
    };
    let m0 = make("main", None, None, None);
    let m1 = make("main", Some(m0.hash.clone()), None, Some("first"));
    let f1 = make("feature-x", Some(m0.hash.clone()), None, None);
    let m2 = make(
        "main",
        Some(m1.hash.clone()),
        Some(f1.hash.clone()),
        Some("Land feature X (fb-432, FB-410; fb-432 again)"),
    );
    let trees: Vec<_> = oak_core::build_tree(&m.entries)
        .unwrap()
        .trees
        .iter()
        .map(oak_core::protocol::tree_to_wire)
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"branches":[{"name":"main","head":m2.hash.to_string()}]}),
        ))
        .mount(&fx.server)
        .await;
    // Serve exactly the requested subset of the remote history, like the
    // real endpoint (unknown hashes are silently omitted).
    let by_hash: std::collections::HashMap<String, serde_json::Value> = [&m2, &m1, &m0, &f1]
        .into_iter()
        .map(|c| {
            let mut data = wire(c);
            data.merge_parent_hash = c.merge_parent_hash.as_ref().map(ToString::to_string);
            data.message = c.message.clone();
            (c.hash.to_string(), serde_json::to_value(data).unwrap())
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let commits: Vec<serde_json::Value> = body["hashes"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|hash| by_hash.get(hash.as_str().unwrap()).cloned())
                .collect();
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"commits": commits, "trees": trees}))
        })
        .with_priority(2)
        .mount(&fx.server)
        .await;
    (fx, vec![m2, m1, m0], f1)
}

async fn remote_log(fx: &Remote, from: Option<&str>, limit: usize) -> serde_json::Value {
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::log_remote::run_json(&fx.root, None, from, Some(limit)).await;
    let captured = oak_cli::output::end_capture();
    result.unwrap();
    serde_json::from_str(captured.trim()).unwrap()
}

#[tokio::test]
async fn fb432_remote_log_walks_verified_first_parents_with_merge_source_and_honest_truncation() {
    let (fx, main, feature) = remote_log_fixture().await;
    let page = remote_log(&fx, None, 2).await;
    assert_eq!(page["kind"], "remote_log");
    assert_eq!(page["head"], main[0].hash.to_string());
    assert_eq!(page["head_source"], "remote_branch_list");
    assert_eq!(page["returned_count"], 2);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["truncation_reason"], "limit");
    assert_eq!(page["complete"], false);
    let rows = page["commits"].as_array().unwrap();
    assert_eq!(rows[0]["hash"], main[0].hash.to_string());
    assert_eq!(rows[0]["merge_parent"], feature.hash.to_string());
    assert_eq!(rows[0]["merge_source_branch"], "feature-x");
    assert_eq!(
        rows[0]["feedback_refs"],
        serde_json::json!(["fb-432", "fb-410"])
    );
    assert_eq!(rows[1]["hash"], main[1].hash.to_string());
    assert!(rows[1].get("merge_source_branch").is_none());
    let next = page["next_command"].as_str().unwrap();
    assert_eq!(
        next,
        format!(
            "oak log --remote --json --branch main --from {} -n 2",
            main[2].hash
        )
    );
    // The limit bounds acquisition too: M0 was never requested.
    assert_eq!(page["acquisition"]["commit_info_requests"], 2);
    // L3: M2, M1 and F1 were fetched and hash-verified.
    assert_eq!(page["acquisition"]["objects_verified"], 3);
    // M1: a continuation always starts strictly below the invoked head.
    assert_ne!(next, "oak log --remote --json --branch main -n 2");
    assert!(!next.contains(main[0].hash.as_str()));

    let rest = remote_log(&fx, Some(main[2].hash.as_str()), 2).await;
    assert_eq!(rest["head_source"], "from_argument");
    assert_eq!(rest["returned_count"], 1);
    assert_eq!(rest["complete"], true);
    assert_eq!(rest["truncated"], false);
    assert!(rest.get("next_command").is_none());

    // Read-only: nothing fetched was persisted, refs untouched.
    for commit in main.iter().chain([&feature]) {
        assert!(fx.repo.get_commit(&commit.hash).unwrap().is_none());
    }
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn fb432_remote_log_rejects_forged_commit_rows() {
    let (fx, main, _) = remote_log_fixture().await;
    // A row whose content does not hash to the requested head is refused.
    let mut forged = wire(&main[0]);
    forged.author = "someone else".into();
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"commits":[forged],"trees":[]})),
        )
        .with_priority(1)
        .mount(&fx.server)
        .await;
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::log_remote::run_json(&fx.root, None, None, Some(3)).await;
    oak_cli::output::end_capture();
    let error = result.unwrap_err().to_string();
    assert!(error.contains("invalid commit object"), "{error}");
}

async fn remote_log_result(
    fx: &Remote,
    from: Option<&str>,
    limit: usize,
) -> oak_core::Result<serde_json::Value> {
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::log_remote::run_json(&fx.root, None, from, Some(limit)).await;
    let captured = oak_cli::output::end_capture();
    result.map(|_| serde_json::from_str(captured.trim()).unwrap())
}

fn invoking_command(from: Option<&str>, limit: usize) -> String {
    match from {
        Some(from) => format!("oak log --remote --json --from {from} -n {limit}"),
        None => format!("oak log --remote --json --branch main -n {limit}"),
    }
}

#[tokio::test]
async fn fb432_unknown_from_is_a_typed_error_and_never_a_self_loop() {
    let (fx, _, _) = remote_log_fixture().await;
    let unknown = "ab".repeat(32);
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(wiremock::matchers::body_json(
            serde_json::json!({"hashes": [unknown.clone()]}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"commits":[],"trees":[]})),
        )
        .with_priority(1)
        .mount(&fx.server)
        .await;
    let error = remote_log_result(&fx, Some(&unknown), 2).await.unwrap_err();
    assert!(matches!(error, oak_core::OakError::Server(_)), "{error:?}");
    assert!(error.to_string().contains("not available"), "{error}");
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn fb432_unavailable_parent_truncates_without_a_retry_command() {
    let (fx, main, feature) = remote_log_fixture().await;
    // The server omits M1 (the first parent) but serves the merge source.
    let mut hashes = vec![main[1].hash.to_string(), feature.hash.to_string()];
    hashes.sort();
    Mock::given(method("POST"))
        .and(path("/api/oak/oak/commits/info"))
        .and(wiremock::matchers::body_json(
            serde_json::json!({"hashes": hashes}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"commits":[wire(&feature)],"trees":[]})),
        )
        .with_priority(1)
        .mount(&fx.server)
        .await;
    let page = remote_log_result(&fx, None, 5).await.unwrap();
    assert_eq!(page["returned_count"], 1);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["truncation_reason"], "commit_unavailable");
    assert_eq!(page["complete"], false);
    assert_eq!(page["commits"][0]["merge_source_branch"], "feature-x");
    assert!(page.get("next_command").is_none(), "{page}");
}

#[tokio::test]
async fn fb432_next_command_never_repeats_the_invocation() {
    let (fx, main, _) = remote_log_fixture().await;
    for from in [
        None,
        Some(main[0].hash.to_string()),
        Some(main[1].hash.to_string()),
    ] {
        for limit in 1..=4 {
            let page = remote_log_result(&fx, from.as_deref(), limit)
                .await
                .unwrap();
            if let Some(next) = page.get("next_command").and_then(|n| n.as_str()) {
                assert_ne!(next, invoking_command(from.as_deref(), limit));
                assert!(!next.contains(page["head"].as_str().unwrap()), "{next}");
            }
        }
    }
}

#[tokio::test]
async fn fb334_budget_followups_keep_line_numbers() {
    let fx = remote().await;
    let (_, out) = fx
        .run(
            &fx.endpoints(),
            true,
            DiffJsonOptions {
                max_bytes: Some(1),
                ..line_numbers()
            },
        )
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(json["hunks_truncated"], true);
    let followups: Vec<&str> = json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .filter(|c| c.contains(" -- "))
        .collect();
    assert!(!followups.is_empty(), "{json}");
    for command in followups {
        assert!(
            command.contains("--json --hunks --line-numbers"),
            "{command}"
        );
    }

    let local = local();
    let json = branch_diff(
        &local,
        DiffMode::Tree,
        DiffJsonOptions {
            max_bytes: Some(1),
            ..line_numbers()
        },
    );
    assert!(json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .filter(|c| c.contains(" -- "))
        .all(|c| c.contains("--line-numbers")));
}
