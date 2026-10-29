//! Bounds and integrity regressions for remote diff hydration and remote file
//! inspect, contributed by independent QA (QA-L6) and adopted as-is.
use oak_core::{
    Branch, Commit, FileMode, Hash, Manifest, ManifestEntry, MetadataKey, Repository,
    SqliteRepository,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct Fx {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    repo: SqliteRepository,
    server: MockServer,
    main: Commit,
    feature: Commit,
}

/// files: (path, main bytes, feature bytes, feature mode)
async fn fixture(files: &[(String, Vec<u8>, Vec<u8>, FileMode)]) -> Fx {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir(root.join(".oak")).unwrap();
    let repo = SqliteRepository::open(&root.join(".oak/oak.db")).unwrap();
    let mut commits: Vec<Commit> = Vec::new();
    for (name, feature) in [("main", false), ("feature", true)] {
        let manifest = Manifest::new(
            files
                .iter()
                .map(|(p, m, f, mode)| ManifestEntry {
                    path: p.clone(),
                    blob_hash: oak_core::hash_bytes(if feature { f } else { m }),
                    mode: if feature { *mode } else { FileMode::Regular },
                })
                .collect(),
        );
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
            None,
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
    Fx {
        _temp: temp,
        root,
        repo,
        server,
        main,
        feature,
    }
}

#[derive(Clone)]
struct Raw {
    /// "/api/oak/oak/raw/{head}/{path}" -> bytes
    map: Arc<HashMap<String, Vec<u8>>>,
    hits: Arc<Mutex<usize>>,
}
impl Respond for Raw {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        *self.hits.lock().unwrap() += 1;
        match self.map.get(request.url.path()) {
            Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
            None => ResponseTemplate::new(404),
        }
    }
}

impl Fx {
    async fn serve(&self, files: &[(String, Vec<u8>, Vec<u8>, FileMode)]) -> Arc<Mutex<usize>> {
        let mut map = HashMap::new();
        for (p, m, f, _) in files {
            map.insert(
                format!("/api/oak/oak/raw/{}/{p}", self.main.hash),
                m.clone(),
            );
            map.insert(
                format!("/api/oak/oak/raw/{}/{p}", self.feature.hash),
                f.clone(),
            );
        }
        let hits = Arc::new(Mutex::new(0));
        Mock::given(method("GET"))
            .and(path_regex("^/api/oak/oak/raw/.*"))
            .respond_with(Raw {
                map: Arc::new(map),
                hits: hits.clone(),
            })
            .mount(&self.server)
            .await;
        hits
    }
    async fn diff(&self) -> serde_json::Value {
        oak_cli::output::begin_capture();
        let result = oak_cli::commands::review::remote_branch_diff_json(
            &self.root,
            "feature",
            "main",
            oak_cli::commands::review::DiffMode::Tree,
            &[],
            oak_cli::commands::review::DiffJsonOptions {
                hunks: true,
                ..Default::default()
            },
        )
        .await;
        let captured = oak_cli::output::end_capture();
        result.unwrap();
        serde_json::from_str(captured.trim()).unwrap()
    }
    fn refs_unchanged(&self) {
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
    }
}

fn many(n: usize) -> Vec<(String, Vec<u8>, Vec<u8>, FileMode)> {
    (0..n)
        .map(|i| {
            (
                format!("f{i:04}.txt"),
                format!("main {i}\n").into_bytes(),
                format!("feature {i}\n").into_bytes(),
                FileMode::Regular,
            )
        })
        .collect()
}

#[tokio::test]
async fn qa_object_cap_is_honoured_and_labelled() {
    // 200 modified files = 400 objects needed; cap is 256 raw reads.
    let files = many(200);
    let fx = fixture(&files).await;
    let hits = fx.serve(&files).await;
    let json = fx.diff().await;
    let raw = *hits.lock().unwrap();
    eprintln!("raw hits = {raw}");
    eprintln!("acquisition = {}", json["acquisition"]);
    assert!(raw <= 256, "raw requests {raw} exceed the 256-object cap");
    assert_eq!(json["acquisition"]["raw_requests"], raw);
    assert_eq!(json["acquisition"]["budget_exhausted"], "object_count");
    let cf = json["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 200);
    let with_patch = cf.iter().filter(|f| f["patch"].is_string()).count();
    let budget = cf
        .iter()
        .filter(|f| f["patch_omitted_reason"] == "remote_content_budget")
        .count();
    eprintln!("with_patch={with_patch} budget={budget}");
    assert_eq!(with_patch + budget, 200, "every omitted file is labelled");
    assert!(with_patch >= 128);
    // No unlabeled omissions, no silent missing_blob.
    for f in cf {
        if f["patch"].is_null() {
            assert_eq!(f["patch_omitted"], true, "{f}");
        }
    }
    let next = json["recommended_next_commands"].as_array().unwrap();
    eprintln!("next[0..3] = {:?}", &next[..next.len().min(3)]);
    eprintln!("next len = {}", next.len());
    eprintln!("caveats = {}", json["caveats"]);
    assert!(json["hunks_truncated"] == true);
    fx.refs_unchanged();
}

#[tokio::test]
async fn qa_diff_hash_mismatch_is_not_stored_and_is_per_file() {
    let mut files = many(3);
    let fx = fixture(&files).await;
    // Serve wrong bytes for the feature side of the middle file.
    let wrong = b"TAMPERED\n".to_vec();
    let good_feature = files[1].2.clone();
    files[1].2 = wrong.clone();
    let _hits = fx.serve(&files).await;
    let json = fx.diff().await;
    let cf = json["changed_files"].as_array().unwrap();
    assert!(cf[0]["patch"].is_string());
    assert!(cf[2]["patch"].is_string(), "later file must still render");
    assert!(cf[1]["patch"].is_null());
    assert_eq!(cf[1]["patch_omitted_reason"], "remote_content_unavailable");
    assert!(!fx.repo.has_blob(&oak_core::hash_bytes(&wrong)).unwrap());
    assert!(!fx
        .repo
        .has_blob(&oak_core::hash_bytes(&good_feature))
        .unwrap());
    let text = json.to_string();
    assert!(!text.contains("TAMPERED"), "unverified bytes displayed");
    let next = json["recommended_next_commands"].as_array().unwrap();
    assert!(
        !next
            .iter()
            .any(|c| c.as_str().unwrap().contains("f0001.txt")),
        "no per-file rerun for unservable content: {next:?}"
    );
    fx.refs_unchanged();
}

async fn inspect(fx: &Fx, at: &str, file: &str, max: u64) -> (bool, serde_json::Value) {
    oak_cli::output::begin_capture();
    let result = oak_cli::commands::file::inspect_remote(&fx.root, at, file, max, true).await;
    let captured = oak_cli::output::end_capture();
    let verified = result.unwrap();
    (verified, serde_json::from_str(captured.trim()).unwrap())
}

#[tokio::test]
async fn qa_inspect_empty_symlink_and_oversize() {
    let files = vec![
        (
            "empty.txt".to_string(),
            b"x\n".to_vec(),
            Vec::new(),
            FileMode::Regular,
        ),
        (
            "link".to_string(),
            b"x\n".to_vec(),
            b"../../etc/passwd".to_vec(),
            FileMode::Symlink,
        ),
        (
            "big.txt".to_string(),
            b"x\n".to_vec(),
            vec![b'a'; 4096],
            FileMode::Regular,
        ),
    ];
    let fx = fixture(&files).await;
    let hits = fx.serve(&files).await;

    let (ok, empty) = inspect(&fx, "feature", "empty.txt", 1 << 20).await;
    eprintln!("empty = {empty}");
    assert!(ok);
    assert_eq!(empty["verified_size"], 0);
    assert_eq!(empty["content"], "");
    assert_eq!(
        *hits.lock().unwrap(),
        0,
        "canonical empty needs no raw read"
    );

    let (ok, link) = inspect(&fx, "feature", "link", 1 << 20).await;
    eprintln!("link = {link}");
    assert!(ok);
    assert_eq!(link["mode"], "Symlink");
    assert_eq!(link["content"], "../../etc/passwd");

    let (ok, big) = inspect(&fx, "feature", "big.txt", 100).await;
    eprintln!("big = {big}");
    assert!(!ok);
    assert_eq!(big["status"], "budget_exceeded");
    assert!(big.get("content").is_none());
    assert!(!fx
        .repo
        .has_blob(&oak_core::hash_bytes(&vec![b'a'; 4096]))
        .unwrap());

    // At the maximum --max-bytes the recommendation must not loop.
    let (_, big_max) = inspect(&fx, "feature", "big.txt", 268_435_456).await;
    assert_eq!(big_max["status"], "verified");

    let (ok, dir) = inspect(&fx, &fx.feature.hash.to_string(), "../big.txt", 1 << 20).await;
    assert!(!ok);
    assert_eq!(dir["status"], "path_missing");
    fx.refs_unchanged();
    let _ = Hash::from_hex(&fx.main.hash.to_string());
}
