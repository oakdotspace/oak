//! fb-197/93/94/328/331/524/430: checkout-free remote reads hydrate exactly the
//! requested content through the verified raw route, report honest per-file
//! omission reasons, and never move refs.
use oak_core::{
    Branch, Commit, FileMode, Manifest, ManifestEntry, MetadataKey, Repository, SqliteRepository,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Paths in changed-file order. `main_side`/`feature_side` are the bytes on
/// each head; none of them is stored locally.
const FILES: &[(&str, &[u8], &[u8])] = &[
    ("a.txt", b"alpha\nbeta\n", b"alpha\nBETA\n"),
    ("b.rs", b"fn b() {}\n", b"fn b() -> u8 { 1 }\n"),
    ("c.md", b"# c\n\nold prose\n", b"# c\n\nnew prose\n"),
];

struct Fixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: SqliteRepository,
    server: MockServer,
    main: Commit,
    feature: Commit,
}

async fn fixture() -> Fixture {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir(root.join(".oak")).unwrap();
    let repo = SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();
    let manifest = |feature: bool| {
        Manifest::new(
            FILES
                .iter()
                .map(|(path, main, branch)| ManifestEntry {
                    path: (*path).into(),
                    blob_hash: oak_core::hash_bytes(if feature { branch } else { main }),
                    mode: FileMode::Regular,
                })
                .collect(),
        )
    };
    let mut commits: Vec<Commit> = Vec::new();
    for (name, feature) in [("main", false), ("feature", true)] {
        let manifest = manifest(feature);
        repo.store_manifest(&manifest).unwrap();
        let commit = Commit::with_timestamp(
            name.into(),
            commits.first().map(|c| c.hash.clone()),
            None,
            manifest.hash,
            "qa".into(),
            None,
            vec![],
            chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        repo.store_branch(&Branch::new(
            name.into(),
            Some("local description".into()),
            (name == "feature").then(|| "main".into()),
        ))
        .unwrap();
        repo.set_branch_head(name, &commit.hash).unwrap();
        commits.push(commit);
    }
    repo.set_current_branch("main").unwrap();
    let server = MockServer::start().await;
    for (key, value) in [
        (MetadataKey::RemoteUrl, server.uri()),
        (MetadataKey::RepoOwner, "oak".into()),
        (MetadataKey::RepoName, "oak".into()),
    ] {
        repo.set_metadata(key, &value).unwrap();
    }
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"branches":[
            {"name":"feature","head":commits[1].hash.to_string(),"parent_branch":"main"},
            {"name":"main","head":commits[0].hash.to_string()}]})),
        )
        .mount(&server)
        .await;
    let feature = commits.pop().unwrap();
    let main = commits.pop().unwrap();
    Fixture {
        _temp: temp,
        root,
        repo,
        server,
        main,
        feature,
    }
}

impl Fixture {
    /// Serve every file's bytes at both heads, except `(head, path)` pairs
    /// listed in `refuse`, which answer with `status`.
    async fn serve_raw(&self, refuse: &[(&Hash, &str)], status: u16) {
        for (head, feature) in [(&self.main.hash, false), (&self.feature.hash, true)] {
            for (file, main, branch) in FILES {
                let route = format!("/api/oak/oak/raw/{head}/{file}");
                let response = if refuse.iter().any(|(h, p)| *h == head && p == file) {
                    ResponseTemplate::new(status).set_body_bytes(b"SECRET_ERROR_BODY".as_slice())
                } else {
                    ResponseTemplate::new(200).set_body_bytes(if feature { *branch } else { *main })
                };
                Mock::given(method("GET"))
                    .and(path(route))
                    .respond_with(response)
                    .mount(&self.server)
                    .await;
            }
        }
    }

    async fn raw_requests(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().contains("/raw/"))
            .count()
    }

    async fn diff(
        &self,
        options: oak_cli::commands::review::DiffJsonOptions,
        paths: &[&str],
    ) -> serde_json::Value {
        let paths: Vec<std::path::PathBuf> = paths.iter().map(Into::into).collect();
        oak_cli::output::begin_capture();
        let result = oak_cli::commands::review::remote_branch_diff_json(
            &self.root,
            "feature",
            "main",
            oak_cli::commands::review::DiffMode::Tree,
            &paths,
            options,
        )
        .await;
        let captured = oak_cli::output::end_capture();
        result.unwrap();
        assert!(!captured.contains("SECRET_ERROR_BODY"));
        serde_json::from_str(captured.trim()).unwrap()
    }

    fn assert_refs_unchanged(&self) {
        assert_eq!(
            self.repo.get_branch_head("feature").unwrap(),
            Some(self.feature.hash.clone())
        );
        assert_eq!(
            self.repo.get_branch_head("main").unwrap(),
            Some(self.main.hash.clone())
        );
        assert_eq!(
            self.repo.get_current_branch_name().unwrap().as_deref(),
            Some("main")
        );
        assert_eq!(
            self.repo
                .get_branch("feature")
                .unwrap()
                .unwrap()
                .description
                .as_deref(),
            Some("local description")
        );
    }
}

use oak_core::Hash;

fn hunks() -> oak_cli::commands::review::DiffJsonOptions {
    oak_cli::commands::review::DiffJsonOptions {
        hunks: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn remote_diff_hunks_hydrate_exact_files_and_warm_rerun_acquires_nothing() {
    let fx = fixture().await;
    fx.serve_raw(&[], 200).await;

    let summary = fx.diff(Default::default(), &[]).await;
    assert_eq!(
        fx.raw_requests().await,
        0,
        "summary-only diff stays metadata-only"
    );
    assert_eq!(summary["acquisition"]["raw_requests"], 0);

    let cold = fx.diff(hunks(), &[]).await;
    assert_eq!(cold["changed_file_count"], 3);
    for file in cold["changed_files"].as_array().unwrap() {
        assert!(file["patch"].is_string(), "{file}");
        assert!(file.get("patch_omitted").is_none(), "{file}");
        assert!(file.get("content_unavailable_reason").is_none(), "{file}");
        assert_eq!(file["additions"], 1);
        assert_eq!(file["deletions"], 1);
    }
    assert!(cold["changed_files"][1]["patch"]
        .as_str()
        .unwrap()
        .contains("+fn b() -> u8 { 1 }"));
    assert_eq!(
        fx.raw_requests().await,
        6,
        "exactly both sides of three files"
    );
    assert_eq!(cold["acquisition"]["raw_requests"], 6);
    assert_eq!(cold["acquisition"]["objects_verified"], 6);
    assert_eq!(cold["acquisition"]["commit_info_requests"], 0);
    assert!(cold.get("hunks_truncated").is_none());

    let warm = fx.diff(hunks(), &[]).await;
    assert_eq!(fx.raw_requests().await, 6, "warm rerun issues no raw reads");
    assert_eq!(warm["acquisition"]["raw_requests"], 0);
    assert_eq!(warm["acquisition"]["commit_info_requests"], 0);
    assert_eq!(warm["acquisition"]["objects_reused"], 6);
    assert_eq!(warm["changed_files"], cold["changed_files"]);
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn path_filtered_remote_hunks_hydrate_only_that_file_and_honor_text_budget() {
    // fb-331: `-a --max-bytes 200000 -- <small file>` returns the patch.
    let fx = fixture().await;
    fx.serve_raw(&[], 200).await;
    let json = fx
        .diff(
            oak_cli::commands::review::DiffJsonOptions {
                hunks: true,
                max_bytes: Some(200_000),
                force_text: true,
                ..Default::default()
            },
            &["b.rs"],
        )
        .await;
    assert_eq!(json["changed_file_count"], 1);
    assert!(json["changed_files"][0]["patch"].is_string());
    assert_eq!(json["changed_files"][0].get("binary_or_large"), None);
    assert_eq!(fx.raw_requests().await, 2);
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn one_unservable_file_never_hides_later_patches_or_loops_recommendations() {
    // fb-328: the refused middle file is omitted with its own reason; the
    // later file still renders; no command re-runs the identical request.
    let fx = fixture().await;
    let feature = fx.feature.hash.clone();
    fx.serve_raw(&[(&feature, "b.rs")], 403).await;
    let json = fx.diff(hunks(), &[]).await;
    let files = json["changed_files"].as_array().unwrap();
    assert!(files[0]["patch"].is_string());
    assert_eq!(files[1]["patch_omitted"], true);
    assert_eq!(
        files[1]["patch_omitted_reason"],
        "remote_content_unavailable"
    );
    assert!(files[2]["patch"].is_string(), "no cascade: {json}");
    assert!(files[2].get("patch_omitted_reason").is_none());
    assert_eq!(json["hunks_truncated"], true);
    let commands = json["recommended_next_commands"].as_array().unwrap();
    assert!(
        !commands
            .iter()
            .any(|c| c.as_str().unwrap().contains("-- b.rs")),
        "unservable content must not be re-requested: {commands:?}"
    );
    assert!(!commands.iter().any(|c| c == "oak fetch"));
    assert!(json["caveats"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c.as_str().unwrap().contains("full ancestry walk")));
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn byte_budget_omissions_are_labeled_and_individually_fetchable() {
    let fx = fixture().await;
    fx.serve_raw(&[], 200).await;
    let json = fx
        .diff(
            oak_cli::commands::review::DiffJsonOptions {
                hunks: true,
                max_bytes: Some(100),
                ..Default::default()
            },
            &[],
        )
        .await;
    let files = json["changed_files"].as_array().unwrap();
    assert!(files[0]["patch"].is_string());
    for file in &files[1..] {
        assert_eq!(file["patch_omitted_reason"], "byte_budget", "{file}");
    }
    let commands: Vec<&str> = json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(commands
        .contains(&"oak branch diff feature --remote --against main --json --hunks -- b.rs"));
}

#[tokio::test]
async fn local_missing_blob_is_per_file_and_points_to_the_remote_variant() {
    let fx = fixture().await;
    // Only the first and last files' content is local; the middle is not.
    for (file, main, branch) in FILES {
        if *file != "b.rs" {
            fx.repo.put_blob(main.to_vec()).unwrap();
            fx.repo.put_blob(branch.to_vec()).unwrap();
        }
    }
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::review::branch_diff_json(
        &fx.root,
        "feature",
        "main",
        oak_cli::commands::review::DiffMode::Tree,
        &[],
        hunks(),
    );
    let captured = oak_cli::output::end_capture();
    result.unwrap();
    let json: serde_json::Value = serde_json::from_str(captured.trim()).unwrap();
    let files = json["changed_files"].as_array().unwrap();
    assert!(files[0]["patch"].is_string());
    assert_eq!(files[1]["patch_omitted_reason"], "missing_blob");
    assert!(files[2]["patch"].is_string(), "no cascade: {json}");
    let commands: Vec<&str> = json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    // QA-L6 M1(b): the branch may never have been pushed, so the remote
    // variant is offered only conditionally, never as a runnable command
    // that could fail with "branch not found".
    assert_eq!(
        commands[0],
        "# 1 file(s) have no local content (missing_blob); if 'feature' is pushed, hydrate them checkout-free with: oak branch diff feature --remote --against main --json --hunks -- <path>"
    );
    assert!(
        !commands
            .iter()
            .any(|c| c.starts_with("oak ") && c.contains("--remote")),
        "{commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| c.starts_with("oak branch diff feature --against main --json")),
        "re-running the local diff cannot fetch anything: {commands:?}"
    );
    assert_eq!(
        fx.raw_requests().await,
        0,
        "local diff never touches the network"
    );
}

#[tokio::test]
async fn remote_print_renders_the_hydrated_patch() {
    let fx = fixture().await;
    fx.serve_raw(&[], 200).await;
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::review::remote_branch_diff_print(
        &fx.root,
        "feature",
        "main",
        oak_cli::commands::review::DiffMode::Tree,
        &["c.md".into()],
        Default::default(),
    )
    .await;
    let captured = oak_cli::output::end_capture();
    result.unwrap();
    assert!(
        captured.starts_with("diff --oak a/c.md b/c.md\n"),
        "{captured}"
    );
    assert!(captured.contains("-old prose\n+new prose"), "{captured}");
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn remote_review_reports_acquisition_and_warm_rerun_is_zero() {
    let fx = fixture().await;
    fx.serve_raw(&[], 200).await;
    let review = || async {
        oak_cli::output::begin_capture();
        let result = oak_cli::commands::review::remote_branch_review_json(
            &fx.root, "feature", true, "main", None, 0,
        )
        .await;
        let captured = oak_cli::output::end_capture();
        result.unwrap();
        serde_json::from_str::<serde_json::Value>(captured.trim()).unwrap()
    };
    let cold = review().await;
    assert_eq!(
        cold["merge_preview"]["prediction_available"], true,
        "{cold}"
    );
    assert!(cold["acquisition"]["raw_requests"].as_u64().unwrap() > 0);
    assert_eq!(
        cold["acquisition"]["raw_requests"],
        cold["acquisition"]["objects_verified"]
    );
    assert!(cold["acquisition"]["bytes_downloaded"].as_u64().unwrap() > 0);
    let warm = review().await;
    assert_eq!(warm["acquisition"]["branch_list_requests"], 1);
    assert_eq!(warm["acquisition"]["raw_requests"], 0, "{warm}");
    assert_eq!(warm["acquisition"]["commit_info_requests"], 0);
    assert_eq!(warm["acquisition"]["bytes_downloaded"], 0);
    assert!(warm["acquisition"].get("budget_exhausted").is_none());
    fx.assert_refs_unchanged();
}

async fn inspect(fx: &Fixture, at: &str, file: &str) -> (bool, serde_json::Value) {
    oak_cli::output::begin_capture();
    let result =
        oak_cli::commands::file::inspect_remote(&fx.root, at, file, 16 * 1024 * 1024, true).await;
    let captured = oak_cli::output::end_capture();
    let verified = result.unwrap();
    assert!(!captured.contains("SECRET_ERROR_BODY"));
    (verified, serde_json::from_str(captured.trim()).unwrap())
}

#[tokio::test]
async fn remote_file_inspect_binds_exact_head_and_verifies_bytes() {
    let fx = fixture().await;
    let main = fx.main.hash.clone();
    fx.serve_raw(&[(&main, "c.md")], 403).await;

    let (verified, json) = inspect(&fx, "feature", "b.rs").await;
    assert!(verified);
    assert_eq!(json["status"], "verified");
    assert_eq!(json["head_source"], "remote_branch_list");
    assert_eq!(json["commit"], fx.feature.hash.to_string());
    assert_eq!(json["blob"], oak_core::hash_bytes(FILES[1].2).to_string());
    assert_eq!(json["content"], "fn b() -> u8 { 1 }\n");
    assert_eq!(json["content_source"], "remote_raw");
    assert_eq!(json["acquisition"]["raw_requests"], 1);

    // Warm: same bytes from the verified cache, no raw read.
    let (_, warm) = inspect(&fx, &fx.feature.hash.to_string(), "b.rs").await;
    assert_eq!(warm["head_source"], "explicit_commit");
    assert_eq!(warm["content_source"], "local_cache");
    assert_eq!(warm["acquisition"]["raw_requests"], 0);
    assert_eq!(fx.raw_requests().await, 1);

    let (verified, denied) = inspect(&fx, "main", "c.md").await;
    assert!(!verified);
    assert_eq!(denied["status"], "access_denied");
    assert!(denied.get("content").is_none());

    let (verified, missing) = inspect(&fx, "main", "nope.txt").await;
    assert!(!verified);
    assert_eq!(missing["status"], "path_missing");
    fx.assert_refs_unchanged();
}

#[tokio::test]
async fn remote_file_inspect_rejects_bytes_that_do_not_match_the_manifest() {
    let fx = fixture().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/oak/oak/raw/{}/a.txt", fx.main.hash)))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"SECRET_ERROR_BODY".as_slice()))
        .mount(&fx.server)
        .await;
    let (verified, json) = inspect(&fx, "main", "a.txt").await;
    assert!(!verified);
    assert_eq!(json["status"], "hash_mismatch");
    assert!(json.get("content").is_none());
    assert!(!fx
        .repo
        .has_blob(&oak_core::hash_bytes(b"SECRET_ERROR_BODY"))
        .unwrap());
}

#[tokio::test]
async fn local_review_and_diff_without_a_remote_never_name_remote_commands() {
    // QA-L6 M1(a): no remote configured -> nothing that needs one.
    let fx = fixture().await;
    for key in [
        MetadataKey::RemoteUrl,
        MetadataKey::RepoOwner,
        MetadataKey::RepoName,
    ] {
        rusqlite::Connection::open(fx.root.join(".oak/oak.db"))
            .unwrap()
            .execute(
                "DELETE FROM metadata WHERE key = ?1",
                rusqlite::params![key.as_str()],
            )
            .unwrap();
    }
    assert!(fx
        .repo
        .get_metadata(MetadataKey::RemoteUrl)
        .unwrap()
        .is_none());
    // Branch content exists except b.rs, and main is an unverified target,
    // so the review is uncertified and the diff has a missing blob.
    for (file, main, branch) in FILES {
        if *file != "b.rs" {
            fx.repo.put_blob(main.to_vec()).unwrap();
            fx.repo.put_blob(branch.to_vec()).unwrap();
        }
    }
    oak_cli::output::begin_capture();
    let review = oak_cli::commands::review::branch_review_json(
        &fx.root, "feature", true, "main", None, 0, None,
    );
    let diff = oak_cli::commands::review::branch_diff_json(
        &fx.root,
        "feature",
        "main",
        oak_cli::commands::review::DiffMode::Tree,
        &[],
        hunks(),
    );
    let captured = oak_cli::output::end_capture();
    review.unwrap();
    diff.unwrap();
    for line in captured.lines() {
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        for command in json["recommended_next_commands"].as_array().unwrap() {
            let command = command.as_str().unwrap();
            assert!(
                !command.contains("--remote") && command != "oak fetch",
                "no-remote repository was told to run {command}: {json}"
            );
        }
        if json.get("merge_preview").is_some() {
            assert_ne!(json["merge_preview"]["merge_safety"]["certified"], true);
            for command in json["merge_preview"]["recommended_next_commands"]
                .as_array()
                .unwrap()
            {
                let command = command.as_str().unwrap();
                assert!(
                    command != "oak fetch" && !command.starts_with("oak pull"),
                    "no-remote preview was told to run {command}"
                );
            }
        }
    }
    assert_eq!(fx.raw_requests().await, 0);
}

#[tokio::test]
async fn remote_inspect_never_repeats_an_unhelpful_budget_suggestion() {
    // QA-L6 L1: a larger --max-bytes is suggested only when it can succeed.
    let fx = fixture().await;
    fx.serve_raw(&[], 200).await;
    let run = |max: u64| {
        let root = fx.root.clone();
        async move {
            oak_cli::output::begin_capture();
            let result =
                oak_cli::commands::file::inspect_remote(&root, "feature", "c.md", max, true).await;
            let captured = oak_cli::output::end_capture();
            result.unwrap();
            serde_json::from_str::<serde_json::Value>(captured.trim()).unwrap()
        }
    };
    let cold = run(4).await;
    assert_eq!(cold["status"], "budget_exceeded");
    assert_eq!(cold["acquisition"]["budget_exhausted"], "bytes");
    assert_eq!(cold["acquisition"]["branch_list_requests"], 1);
    assert_eq!(
        cold["recommended_next_commands"][0],
        format!(
            "oak file inspect --remote --at {} c.md --max-bytes 268435456 --json",
            fx.feature.hash
        )
    );
    // Once the bytes are verified and cached, the exact size is known.
    assert_eq!(run(1 << 20).await["status"], "verified");
    let sized = run(4).await;
    assert_eq!(
        sized["recommended_next_commands"][0],
        format!(
            "oak file inspect --remote --at {} c.md --max-bytes {} --json",
            fx.feature.hash,
            FILES[2].2.len()
        )
    );
}
