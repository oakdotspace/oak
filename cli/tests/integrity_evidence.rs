use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn healthy_report() -> Value {
    json!({
        "schema_version": 1,
        "repo": "oak/repo",
        "status": "healthy",
        "healthy": true,
        "verification": "metadata",
        "scope": {"commit_count": 0, "manifest_count": 0, "blob_count": 0, "chunk_count": 0}
    })
}

async fn diagnostic(report: Value, arguments: &[&str]) -> std::process::Output {
    diagnostic_for_repo(report, arguments, "oak/repo").await
}

async fn diagnostic_for_repo(
    report: Value,
    arguments: &[&str],
    repo: &str,
) -> std::process::Output {
    // An owned listener avoids sharing a fixture address across runtimes.
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(report))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(arguments)
            .args(["--remote", &server.uri(), "--repo", repo, "--json"])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env_remove("OAK_API_KEY")
            .env("OAK_NO_UPDATE_CHECK", "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("bounded CLI process")
    .expect("run CLI")
}

#[tokio::test]
async fn doctor_rejects_another_repository_before_emitting_a_healthy_report() {
    let mut report = healthy_report();
    report["repo"] = json!("other/repository");
    let output = diagnostic(report, &["doctor", "--verify", "metadata"]).await;
    assert!(!output.status.success(), "{:?}", output);
    let body: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_ne!(body["healthy"], json!(true));
    assert!(body.to_string().contains("repository"));
}

#[tokio::test]
async fn blob_info_rejects_evidence_for_another_blob() {
    let mut report = healthy_report();
    report["verification"] = json!("bytes");
    report["blob_evidence"] = json!([{
        "hash": "bb".repeat(32), "metadata_present": true, "mapping_present": true
    }]);
    let output = diagnostic(report, &["blob", "info", &"aa".repeat(32)]).await;
    assert!(!output.status.success(), "{:?}", output);
    let body: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_ne!(body["healthy"], json!(true));
    assert!(body["error"]["message"].as_str().unwrap().contains("blob"));
}

#[tokio::test]
async fn blob_info_success_requires_exactly_one_target_evidence_record() {
    let target = "aa".repeat(32);
    let evidence = json!({"hash": target, "metadata_present": true, "mapping_present": true});
    for history_only in [false, true] {
        for records in [json!([]), json!([evidence.clone(), evidence.clone()])] {
            let mut report = healthy_report();
            report["verification"] = json!("bytes");
            report["blob_evidence"] = records;
            if history_only {
                report["healthy"] = json!(false);
                report["complete"] = json!(false);
                report["truncated"] = json!(true);
                report["status"] = json!("content_incomplete");
                report["findings"] = json!([{
                    "code": "target_history_budget_exhausted",
                    "recoverability": "requires_authoritative_bytes", "detail": "bounded history"
                }]);
            }
            let output = diagnostic(report, &["blob", "info", &target]).await;
            assert!(!output.status.success(), "{:?}", output);
            let body: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
            assert!(body["error"]["message"].as_str().unwrap().contains("blob"));
        }
    }
}

async fn clone_preflight(report: Value, arguments: &[&str]) -> std::process::Output {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity/capabilities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "clone_preflight_profile": "bounded_v1",
            "selected_branch_acquisition": "exact_head_v1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oak/repo/integrity"))
        .respond_with(ResponseTemplate::new(200).set_body_json(report))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().unwrap();
    let destination = home.path().join("destination");
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args([
                "clone",
                "oak/repo",
                destination.to_str().unwrap(),
                "--remote",
                &server.uri(),
            ])
            .args(arguments)
            .current_dir(home.path())
            .env("HOME", home.path())
            .env_remove("OAK_API_KEY")
            .env("OAK_NO_UPDATE_CHECK", "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("bounded clone process")
    .expect("run clone");
    assert!(
        !destination.exists(),
        "negative preflight creates no destination"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "no pull or mutation after negative preflight"
    );
    output
}

fn wall_budget_report() -> Value {
    // Actual hosted wall-time fallback: negotiation echo, but neither a
    // snapshot token nor selected-branch evidence was completed.
    let mut report = healthy_report();
    report["healthy"] = json!(false);
    report["complete"] = json!(false);
    report["truncated"] = json!(true);
    report["status"] = json!("content_incomplete");
    report["proof_profile"] = json!("bounded_v1");
    report["known_loss_protocol"] = json!("report_v1");
    report["findings"] = json!([{
        "code": "clone_preflight_wall_budget_exhausted",
        "recoverability": "requires_authoritative_bytes",
        "detail": "integrity inspection exceeded its request wall budget"
    }]);
    report
}

#[tokio::test]
async fn clone_preserves_wall_budget_diagnosis_without_admission_evidence() {
    let output = clone_preflight(
        wall_budget_report(),
        &["--branch", "feature", "--expected-head", &"aa".repeat(32)],
    )
    .await;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("clone_preflight_wall_budget_exhausted"),
        "{error}"
    );
    assert!(!error.contains("invalid known-loss proof"), "{error}");
    assert!(!error.contains("omitted its snapshot token"), "{error}");
}

#[tokio::test]
async fn budget_advice_does_not_offer_to_waive_missing_admission_evidence() {
    let head = "aa".repeat(32);
    let branch = "feature; echo 'not-a-command'";
    let output = clone_preflight(
        wall_budget_report(),
        &[
            "--branch",
            branch,
            "--expected-head",
            &head,
            "--path",
            "src/",
        ],
    )
    .await;
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("clone_preflight_wall_budget_exhausted"),
        "{error}"
    );
    assert!(!error.contains("--allow-unverified-integrity"), "{error}");
    assert!(error.contains("--expected-head"), "{error}");
    assert!(error.contains("--path src "), "{error}");
    assert!(
        error.contains("'feature; echo '\\''not-a-command'\\'''"),
        "{error}"
    );
}

#[tokio::test]
async fn diagnostic_success_cannot_silently_change_requested_history_scope() {
    let mut report = healthy_report();
    report["scope"]["depth"] = json!(1);
    let output = diagnostic(report, &["doctor", "--verify", "metadata"]).await;
    assert!(!output.status.success(), "{:?}", output);
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("scope"));

    let target = "aa".repeat(32);
    let mut report = healthy_report();
    report["verification"] = json!("bytes");
    report["scope"]["depth"] = json!(1);
    report["blob_evidence"] = json!([{
        "hash": target, "metadata_present": true, "mapping_present": true
    }]);
    let output = diagnostic(report, &["blob", "info", &target, "--depth", "2"]).await;
    assert!(!output.status.success(), "{:?}", output);
}

#[tokio::test]
async fn mixed_budget_content_failures_keep_the_content_reason_and_cannot_be_waived() {
    for code in ["missing_blob_mapping", "known_lost_blob"] {
        let mut report = wall_budget_report();
        report["findings"].as_array_mut().unwrap().push(json!({
            "code": code, "blob_hash": "aa".repeat(32),
            "recoverability": if code == "known_lost_blob" {
                "operator_adjudicated_loss"
            } else { "requires_authoritative_bytes" },
            "detail": "content is unavailable"
        }));
        let output = clone_preflight(report, &["--allow-unverified-integrity"]).await;
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(code), "{error}");
        assert!(!error.contains("invalid known-loss proof"), "{error}");
        assert!(!error.contains("--allow-unverified-integrity"), "{error}");
    }
}

#[tokio::test]
async fn diagnostic_legacy_defaults_and_incomplete_evidence_remain_usable() {
    let output = diagnostic(healthy_report(), &["doctor", "--verify", "metadata"]).await;
    assert!(output.status.success(), "{:?}", output);
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["healthy"], json!(true));

    // The canonical empty blob legitimately has no physical chunks.
    let hash = oak_core::hash_bytes(b"").to_string();
    let mut report = healthy_report();
    report["verification"] = json!("bytes");
    report["blob_evidence"] = json!([{
        "hash": hash, "metadata_present": true, "mapping_present": true
    }]);
    let output = diagnostic(report.clone(), &["blob", "info", &hash]).await;
    assert!(output.status.success(), "{:?}", output);

    report["healthy"] = json!(false);
    report["complete"] = json!(false);
    report["truncated"] = json!(true);
    report["status"] = json!("content_incomplete");
    report["findings"] = json!([{
        "code": "target_history_budget_exhausted", "recoverability": "unknown",
        "detail": "verified target, bounded history"
    }]);
    let output = diagnostic(report.clone(), &["blob", "info", &hash]).await;
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["complete"],
        json!(false)
    );

    // Wall-time exhaustion may occur before any target evidence exists.
    report["blob_evidence"] = json!([]);
    report["findings"][0]["code"] = json!("target_wall_budget_exhausted");
    let output = diagnostic(report, &["blob", "info", &hash]).await;
    assert!(!output.status.success());
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        body["findings"][0]["code"],
        json!("target_wall_budget_exhausted")
    );
}

#[tokio::test]
async fn every_clone_admission_path_still_requires_its_evidence() {
    let mut healthy = healthy_report();
    healthy["proof_profile"] = json!("bounded_v1");
    let output = clone_preflight(healthy.clone(), &[]).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("snapshot token"));

    let output = clone_preflight(wall_budget_report(), &["--allow-unverified-integrity"]).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("snapshot token"));

    let mut budget = wall_budget_report();
    budget["snapshot_token"] = json!("snapshot");
    let output = clone_preflight(
        budget,
        &["--branch", "feature", "--allow-unverified-integrity"],
    )
    .await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("selected branch identity"));

    let mut loss = healthy.clone();
    loss["healthy"] = json!(false);
    loss["status"] = json!("content_incomplete");
    loss["known_loss_protocol"] = json!("report_v1");
    loss["findings"] = json!([{
        "code": "known_lost_blob", "blob_hash": "aa".repeat(32),
        "recoverability": "operator_adjudicated_loss", "detail": "historical loss"
    }]);
    let output = clone_preflight(loss, &[]).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("snapshot token"));

    healthy["snapshot_token"] = json!("snapshot");
    healthy["scope"]["depth"] = json!(1);
    let output = clone_preflight(healthy, &[]).await;
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid integrity scope"));
}

#[tokio::test]
async fn blob_success_cannot_contradict_explicit_negative_target_evidence() {
    let hash = "aa".repeat(32);
    for negative in [
        "metadata_present",
        "mapping_present",
        "object_present",
        "hash_verified",
    ] {
        let mut evidence = json!({
            "hash": hash, "metadata_present": true, "mapping_present": true,
            "chunks": [{"hash": "bb".repeat(32), "offset": 0, "size": 1,
                "metadata_present": true, "object_present": true, "hash_verified": true}]
        });
        if matches!(negative, "object_present" | "hash_verified") {
            evidence["chunks"][0][negative] = json!(false);
        } else {
            evidence[negative] = json!(false);
        }
        let mut report = healthy_report();
        report["verification"] = json!("bytes");
        report["blob_evidence"] = json!([evidence]);
        let output = diagnostic(report, &["blob", "info", &hash]).await;
        assert!(!output.status.success(), "{negative}: {:?}", output);
    }
}

#[tokio::test]
async fn repository_identity_uses_the_normalized_request_subject() {
    let output = diagnostic_for_repo(
        healthy_report(),
        &["doctor", "--verify", "metadata"],
        " oak / repo ",
    )
    .await;
    assert!(output.status.success(), "{:?}", output);
}
