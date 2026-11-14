//! `oak branch diff/review --remote` through the server's checkout-free
//! review API (oakspace `docs/checkout-free-review-api.md`): no checkout, no
//! object download, pins carried from the first response into every
//! follow-up call, and the local path only when the server predates the API.
use oak_cli::commands::review::{server_review, DiffJsonOptions, DiffMode};
use oak_core::{Branch, Commit, Manifest, MetadataKey, Repository, SqliteRepository};
use serde_json::{json, Value};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BH: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const TH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const BASE: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn pins() -> Value {
    json!({ "branch_head": BH, "target_head": TH, "diff_base": BASE })
}

fn diff_page(mode: &str, files: Value) -> Value {
    json!({
        "schema_version": 1, "branch": "feature", "against": "main", "mode": mode,
        "pins": pins(), "heads_current": true,
        "current_branch_head": null, "current_target_head": null,
        "branch_commit_count": 2, "branch_commit_count_exact": true,
        "changed_file_count": files.as_array().unwrap().len(),
        "restricted_paths_omitted": 1,
        "files": files,
        "patch_budget_exhausted": false, "byte_budget_exhausted": false,
        "time_budget_exhausted": false, "next_cursor": null,
        "conflict_file_count": 0, "conflict_files": [], "conflicts_restricted": 0,
    })
}

fn row(path: &str, state: &str, patch: Option<&str>) -> Value {
    json!({
        "path": path, "change_type": "modified", "old_path": null,
        "content_state": state, "detail": null, "size_old": 10, "size_new": 12,
        "additions": if state == "text" { json!(1) } else { Value::Null },
        "deletions": if state == "text" { json!(1) } else { Value::Null },
        "patch": patch, "patch_omitted": false,
    })
}

const PATCH: &str = "diff --oak a/docs/guide.md b/docs/guide.md\n--- a/docs/guide.md\n+++ b/docs/guide.md\n@@ -1 +1 @@\n-old\n+new\n";

async fn mount_diff(server: &MockServer, mode: &str, content_param: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/feature/diff"))
        .and(query_param("mode", mode))
        .and(query_param(content_param, "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .with_priority(1)
        .mount(server)
        .await;
}

async fn mount_listing(server: &MockServer, mode: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/feature/diff"))
        .and(query_param("mode", mode))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .with_priority(5)
        .mount(server)
        .await;
}

async fn capture<F: std::future::Future<Output = oak_core::Result<()>>>(
    f: F,
) -> oak_core::Result<Value> {
    oak_cli::output::begin_capture();
    let result = f.await;
    let captured = oak_cli::output::end_capture();
    result.map(|_| serde_json::from_str(captured.trim()).unwrap())
}

fn non_review_requests(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .map(|r| r.url.path().to_string())
        .filter(|p| !p.contains("/branches/feature/diff") && !p.contains("/merge-preview"))
        .collect()
}

#[tokio::test]
async fn diff_without_a_checkout_uses_the_review_api_and_pins_follow_ups() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = MockServer::start().await;
    mount_listing(
        &server,
        "tree",
        diff_page("tree", json!([row("docs/guide.md", "not_computed", None)])),
    )
    .await;
    mount_diff(
        &server,
        "tree",
        "patches",
        diff_page("tree", json!([row("docs/guide.md", "text", Some(PATCH))])),
    )
    .await;
    let empty = tempfile::tempdir().unwrap();
    let repo = format!("{}/oak/oak", server.uri());

    let json = capture(server_review::branch_diff_json(
        empty.path(),
        Some(&repo),
        "feature",
        "main",
        DiffMode::Tree,
        &[],
        DiffJsonOptions {
            hunks: true,
            ..Default::default()
        },
    ))
    .await
    .unwrap();

    assert_eq!(json["kind"], "remote_branch_diff");
    assert_eq!(json["branch_head"], BH);
    assert_eq!(json["evidence_kind"], "snapshot");
    let file = &json["changed_files"][0];
    assert_eq!(file["path"], "docs/guide.md");
    assert_eq!(file["patch"], PATCH);
    assert_eq!(file["additions"], 1);
    assert!(
        file.get("binary_or_large").is_none(),
        "a readable text file is not binary_or_large"
    );
    let caveats = json["caveats"].to_string();
    assert!(caveats.contains("no local checkout"), "{caveats}");
    assert!(caveats.contains("hidden by this repository's path permissions"));
    // `--repo` rides along on suggested remote commands.
    assert!(json["recommended_next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c.as_str().unwrap().contains("--remote"))
        .all(|c| c.as_str().unwrap().contains("--repo")));

    // Exactly two calls (listing + content), the second pinned; nothing else.
    let requests = server.received_requests().await.unwrap();
    assert!(non_review_requests(&requests).is_empty(), "{requests:?}");
    assert_eq!(requests.len(), 2);
    let content = requests[1].url.query().unwrap_or_default().to_string();
    assert!(content.contains(&format!("branch_head={BH}")), "{content}");
    assert!(content.contains(&format!("target_head={TH}")), "{content}");
    assert!(content.contains("paths=docs%2Fguide.md"), "{content}");
}

#[tokio::test]
async fn review_maps_the_server_verdict() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = MockServer::start().await;
    mount_listing(
        &server,
        "tree",
        diff_page("tree", json!([row("src/lib.rs", "text", None)])),
    )
    .await;
    let mut net = row("src/lib.rs", "not_computed", None);
    net["merge_safety"] = json!("branch_change");
    mount_listing(&server, "net-merge", diff_page("net-merge", json!([net]))).await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/feature/merge-preview"))
        .and(query_param("branch_head", BH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version": 1, "branch": "feature", "target": "main", "pins": pins(),
            "heads_current": true, "current_branch_head": null, "current_target_head": null,
            "prediction": "available", "reason": null, "clean": true,
            "conflict_file_count": 0, "conflict_files": [], "conflict_files_truncated": false,
            "conflicts_restricted": 0, "conflicts_undetermined_unreadable": 0,
            "restricted_paths_omitted": 0, "invariant_violations": [],
            "invariant_violations_truncated": false,
            "merge_safety": {
                "certified": true, "verdict": "safe", "uncertified_cause": null,
                "target_head_source": "server_live", "assessed_target": "main",
                "fork_base_commit": BASE, "branch_head": BH, "target_head": TH,
                "invariant_violation_count": 0, "invariant_violations_restricted": 0,
                "invariant_violations_sample": [], "overlap_count": 0,
                "overlap_paths_sample": [], "violations_digest": "ab".repeat(32),
                "ack_key_id": "0123456789abcdef", "classifier_version": 1, "reasons": []
            },
            "mergeable_cache": "populated"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/ci/checks"))
        .and(query_param("branch", "feature"))
        .and(query_param("commit", BH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "schema_version":1,"branch":"feature","commit":BH,
            "publication_bound":true,"state":"success","gate_passed":true
        })))
        .expect(1)
        .mount(&server)
        .await;
    // This endpoint can contain hundreds of unrelated runs; exact review
    // must not scan it or infer completeness from its window.
    Mock::given(path("/api/oak/oak/ci/runs"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let empty = tempfile::tempdir().unwrap();
    let repo = format!("{}/oak/oak", server.uri());

    let json = capture(server_review::branch_review_json(
        empty.path(),
        Some(&repo),
        "feature",
        true,
        "main",
        None,
        0,
    ))
    .await
    .unwrap();

    assert_eq!(json["branch_head"], BH);
    assert_eq!(json["changed_files"][0]["path"], "src/lib.rs");
    assert_eq!(json["changed_files"][0]["basis"], "snapshot_difference");
    let preview = &json["merge_preview"];
    assert_eq!(preview["prediction_method"], "server_review_api");
    assert_eq!(preview["clean"], true);
    assert_eq!(preview["merge_safety"]["verdict"], "safe");
    assert_eq!(preview["merge_safety"]["target_head_source"], "server_live");
    assert_eq!(
        preview["merge_safety"]["violations_digest"],
        "ab".repeat(32)
    );
    assert_eq!(preview["merge_safety"]["ack_key_id"], "0123456789abcdef");
    assert_eq!(json["mergeability"], "clean");
    assert_eq!(json["contribution"], "contributes");
    assert_eq!(json["vcs_merge_safe"], true);
    assert_eq!(json["checks"]["state"], "success");
    assert_eq!(json["checks"]["source"], "server_exact_publication_gate");
    assert_eq!(json["checks"]["head"], BH);
    assert_eq!(json["merge_allowed"], true);
}

/// A server without the review routes answers a bare 404: with a checkout
/// the CLI computes locally; without one it says why it can't.
#[tokio::test]
async fn older_server_falls_back_only_with_a_checkout() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = MockServer::start().await;

    let empty = tempfile::tempdir().unwrap();
    let repo = format!("{}/oak/oak", server.uri());
    let err = capture(server_review::branch_diff_json(
        empty.path(),
        Some(&repo),
        "feature",
        "main",
        DiffMode::Tree,
        &[],
        DiffJsonOptions::default(),
    ))
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("does not serve the checkout-free review API"),
        "{err}"
    );

    // A checkout whose remote is the same old server: the local path runs.
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".oak")).unwrap();
    let local = SqliteRepository::open(&temp.path().join(".oak/oak.db")).unwrap();
    let manifest = Manifest::new(Vec::new());
    local.store_manifest(&manifest).unwrap();
    let commit = Commit::new(
        "main".into(),
        None,
        None,
        manifest.hash,
        "qa".into(),
        None,
        vec![],
    )
    .unwrap();
    local.store_commit(&commit).unwrap();
    for name in ["main", "feature"] {
        local
            .store_branch(&Branch::new(
                name.into(),
                None,
                (name == "feature").then(|| "main".into()),
            ))
            .unwrap();
        local.set_branch_head(name, &commit.hash).unwrap();
    }
    local.set_current_branch("main").unwrap();
    for (key, value) in [
        (MetadataKey::RemoteUrl, server.uri()),
        (MetadataKey::RepoOwner, "oak".into()),
        (MetadataKey::RepoName, "oak".into()),
    ] {
        local.set_metadata(key, &value).unwrap();
    }
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"branches":[
            {"name":"feature","head":commit.hash.to_string(),"parent_branch":"main"},
            {"name":"main","head":commit.hash.to_string()}]})))
        .mount(&server)
        .await;
    let json = capture(server_review::branch_diff_json(
        temp.path(),
        None,
        "feature",
        "main",
        DiffMode::Tree,
        &[],
        DiffJsonOptions::default(),
    ))
    .await
    .unwrap();
    assert!(json.get("acquisition").is_some(), "local path ran: {json}");
}

/// The review API's own 404 is a real answer, not an old server.
#[tokio::test]
async fn coded_not_found_is_branch_not_found() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/oak/branches/feature/diff"))
        .respond_with(ResponseTemplate::new(404).set_body_json(
            json!({"error": "Branch 'feature' not found", "error_code": "not_found"}),
        ))
        .mount(&server)
        .await;
    let empty = tempfile::tempdir().unwrap();
    let repo = format!("{}/oak/oak", server.uri());
    let err = capture(server_review::branch_diff_json(
        empty.path(),
        Some(&repo),
        "feature",
        "main",
        DiffMode::Tree,
        &[],
        DiffJsonOptions::default(),
    ))
    .await
    .unwrap_err();
    assert!(
        matches!(err, oak_core::OakError::BranchNotFound(_)),
        "{err}"
    );
}

/// Null target is meaningful only with explicit, bound initialization proof.
#[tokio::test]
async fn first_main_initialization_proof_controls_merge_recommendation() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    for (assessment, ci_state, gate_passed, allowed) in [
        (Some("target_initialization"), "no_runs", true, true),
        (Some("target_initialization"), "no_runs", false, false),
        (Some("target_initialization"), "running", true, false),
        (Some("target_initialization"), "failure", true, false),
        (Some("target_initialization"), "running", false, false),
        (Some("target_initialization"), "failure", false, false),
        (None, "no_runs", true, false),
        (Some("four_tree"), "no_runs", true, false),
    ] {
        let server = MockServer::start().await;
        let mut null_pins = pins();
        null_pins["target_head"] = Value::Null;
        null_pins["diff_base"] = Value::Null;
        for mode in ["tree", "net-merge"] {
            let mut page = diff_page(mode, json!([row("README.md", "text", None)]));
            page["pins"] = null_pins.clone();
            mount_listing(&server, mode, page).await;
        }
        Mock::given(path("/api/oak/oak/branches/feature/merge-preview"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pins": null_pins, "heads_current": true,
                "prediction":"available", "clean":true, "conflict_file_count":0, "conflict_files":[],
                "merge_safety": {
                    "certified":true, "verdict":"safe", "assessment_kind":assessment,
                    "target_head_source":"server_live", "assessed_target":"main",
                    "branch_head":BH, "target_head":null, "fork_base_commit":null,
                    "invariant_violation_count":0, "classifier_version":1
                }
            })))
            .mount(&server).await;
        Mock::given(path("/api/oak/oak/ci/checks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "schema_version":1,"branch":"feature","commit":BH,
                "publication_bound":true,"state":ci_state,"gate_passed":gate_passed
            })))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let repo = format!("{}/oak/oak", server.uri());
        let value = capture(server_review::branch_review_json(
            temp.path(),
            Some(&repo),
            "feature",
            true,
            "main",
            None,
            0,
        ))
        .await
        .unwrap();
        assert_eq!(
            value["merge_allowed"], allowed,
            "{assessment:?}/{ci_state}: {value}"
        );
        if allowed {
            assert_eq!(
                value["merge_preview"]["merge_safety"]["assessment_kind"],
                "target_initialization"
            );
        }
        for request in server.received_requests().await.unwrap() {
            if request.url.path().ends_with("merge-preview")
                || (request.url.path().ends_with("diff")
                    && request
                        .url
                        .query_pairs()
                        .any(|(k, v)| k == "mode" && v == "net-merge"))
            {
                assert!(request
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "target_head_absent" && v == "1"));
            }
        }
    }
}

#[tokio::test]
async fn review_rejects_a_null_target_that_changes_between_server_responses() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let server = MockServer::start().await;
    let mut initial = diff_page("tree", json!([row("README.md", "text", None)]));
    initial["pins"]["target_head"] = Value::Null;
    initial["pins"]["diff_base"] = Value::Null;
    mount_listing(&server, "tree", initial).await;
    // An older server may ignore the additive absent-head pin. Never combine
    // its later initialized target with the first empty-target observation.
    mount_listing(&server, "net-merge", diff_page("net-merge", json!([]))).await;
    let temp = tempfile::tempdir().unwrap();
    let repo = format!("{}/oak/oak", server.uri());
    let error = capture(server_review::branch_review_json(
        temp.path(),
        Some(&repo),
        "feature",
        false,
        "main",
        None,
        0,
    ))
    .await
    .unwrap_err();
    assert!(error.to_string().contains("heads changed"), "{error}");
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}
