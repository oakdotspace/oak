//! Bounded content preparation for one immutable remote review pair.
//! The shared local/batch merge resolver deliberately retains its strict guard.
use super::*;
use crate::commands::blob_fetch::ReviewPreparationBudget;
use crate::commands::branch::RemoteIdentity;
use futures_util::StreamExt;
use std::collections::BTreeMap;

/// Constructed only after all content read by the remote preview is present.
pub(super) struct VerifiedContent {
    base: Manifest,
}

impl VerifiedContent {
    pub(super) fn base(&self) -> &Manifest {
        &self.base
    }
}

fn snapshot_requirements(comparison: &BranchComparison) -> BTreeMap<String, (Hash, String)> {
    let branch = &comparison.branch_manifest;
    let parent = &comparison.comparison_manifest;
    let branch_paths: HashMap<_, _> = branch.entries.iter().map(|e| (&e.path, e)).collect();
    let parent_paths: HashMap<_, _> = parent.entries.iter().map(|e| (&e.path, e)).collect();
    let mut required = BTreeMap::new();
    for (manifest, other, head) in [
        (branch, &parent_paths, comparison.branch_head.as_ref()),
        (parent, &branch_paths, comparison.against_head.as_ref()),
    ] {
        for entry in &manifest.entries {
            if entry_changed(other.get(&entry.path).copied(), Some(entry)) {
                if let Some(head) = head {
                    required
                        .entry(entry.blob_hash.to_string())
                        .or_insert_with(|| (head.clone(), entry.path.clone()));
                }
            }
        }
    }
    required
}

/// Snapshot summary evidence is independent of merge-base availability.
/// This fallback only validates cached M/F bytes; it never fetches or certifies.
pub(super) fn snapshot_content_verified(
    repo: &dyn Repository,
    comparison: &BranchComparison,
) -> Result<bool> {
    if !comparison.branch_snapshot_available || !comparison.comparison_snapshot_available {
        return Ok(false);
    }
    for hash in snapshot_requirements(comparison).keys() {
        oak_core::ensure_empty_blob(repo, &Hash(hash.clone()))?;
        if repo
            .get_blob(&Hash(hash.clone()))?
            .is_none_or(|blob| oak_core::hash_bytes(&blob.content).as_str() != hash)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// After failed preparation, retain structural evidence without reading any
/// unverified bytes. Exact-hash renames are structural; similarity is unknown.
pub(super) fn unverified_tree_evidence(
    comparison: &BranchComparison,
    against: &str,
) -> TreeDiffEvidence {
    let caveat = "Required remote review content is unverified; file statistics and content-similarity rename evidence are unavailable.".to_string();
    if !comparison.branch_snapshot_available || !comparison.comparison_snapshot_available {
        return TreeDiffEvidence {
            changes: Vec::new(),
            changed_files: Vec::new(),
            changed_file_count: 0,
            identical: identical_files_unavailable(against, vec![caveat.clone()]),
            extra_caveats: vec![caveat],
        };
    }
    let changes = comparison
        .comparison_manifest
        .diff(&comparison.branch_manifest);
    let changed_files = changes
        .iter()
        .map(|change| {
            let mut summary = file_summary(change, None, None);
            summary.content_unavailable_reason = Some("unverified_content");
            summary
        })
        .collect();
    TreeDiffEvidence {
        changed_file_count: changes.len(),
        changes,
        changed_files,
        identical: identical_files(
            &comparison.branch_manifest,
            &comparison.comparison_manifest,
            against,
        ),
        extra_caveats: vec![caveat],
    }
}

pub(super) async fn prepare(
    repo: &dyn Repository,
    remote: &RemoteIdentity,
    source: &RemoteAnalysisBranch,
    target: &RemoteAnalysisBranch,
    comparison: &BranchComparison,
    budget: &mut ReviewPreparationBudget,
) -> Result<Option<VerifiedContent>> {
    if source.parent.as_deref() != Some(target.name.as_str())
        || source.head.is_none()
        || target.head.is_none()
        || !comparison.branch_snapshot_available
        || !comparison.comparison_snapshot_available
    {
        return Ok(None);
    }
    let base_head = match crate::commands::merge::resolve_merge_base_identity_from_heads(
        repo,
        &source.name,
        source.head.as_ref(),
        source.parent.as_deref(),
        &target.name,
        target.head.as_ref(),
    )? {
        crate::commands::merge::MergeBaseIdentity::Commit(hash) => Some(hash),
        crate::commands::merge::MergeBaseIdentity::DeclaredRoot => None,
        crate::commands::merge::MergeBaseIdentity::Unavailable(_) => return Ok(None),
    };
    let base = match manifest_for_head(repo, base_head.as_ref()) {
        Ok(base) => base,
        Err(error) if is_local_merge_data_unavailable(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let branch = &comparison.branch_manifest;
    let parent = &comparison.comparison_manifest;
    // The public top-level summary compares M -> F, not fork -> F. Include
    // every endpoint of that diff (also the candidates for similarity renames).
    // M -> predicted-merge summaries consume a subset of these plus generated
    // in-memory merge blobs. The independently resolved lineage fork is used
    // only for manifest-signature merge-safety classification, not blob reads.
    budget.check_deadline()?;
    let mut required = snapshot_requirements(comparison);
    budget.check_deadline()?;
    // Content prediction and target-risk classification read conflict triples.
    // Never let a missing required base become predict_conflicts' empty default.
    let base_paths: HashMap<_, _> = base.entries.iter().map(|e| (&e.path, e)).collect();
    for conflict in three_way_merge_manifests(&base, branch, parent).conflicts {
        for (entry, head) in [
            (base_paths.get(&conflict.path).copied(), base_head.as_ref()),
            (conflict.branch_entry.as_ref(), source.head.as_ref()),
            (conflict.parent_entry.as_ref(), target.head.as_ref()),
        ] {
            if let (Some(entry), Some(head)) = (entry, head) {
                required
                    .entry(entry.blob_hash.to_string())
                    .or_insert_with(|| (head.clone(), entry.path.clone()));
            }
        }
    }
    let restricted = crate::commands::restricted::load_restricted_blobs(repo);
    let lost = crate::commands::known_loss::load_known_lost_blobs(repo);
    let mut missing = Vec::new();
    for (hash, location) in &required {
        budget.check_deadline()?;
        if !repo.has_blob(&Hash(hash.clone()))? {
            // Canonical empty content is implied by its hash, even when an
            // older server omitted the optional blob row. No raw read is needed.
            if oak_core::ensure_empty_blob(repo, &Hash(hash.clone()))? {
                continue;
            }
            // Markers are not proof of content, and do not authorize an access
            // bypass. A stale marker is inert only when the blob exists.
            if restricted.contains(hash) || lost.contains(hash) {
                return Ok(None);
            }
            missing.push((Hash(hash.clone()), location));
        }
    }
    if missing.len() > 256 {
        return Ok(None);
    }
    let client = crate::http::api_client(); // no redirects or credential forwarding
    for (hash, (head, path)) in missing {
        let Some(bytes) = read_raw(&client, remote, head, path, &hash, budget).await else {
            return Ok(None);
        };
        // read_raw verified the frozen manifest hash, never an advertised hash.
        repo.put_blob(bytes)?;
    }
    // Recheck the complete requirement set before admitting any content reader.
    for hash in required.keys() {
        budget.check_deadline()?;
        if repo
            .get_blob(&Hash(hash.clone()))?
            .is_none_or(|blob| oak_core::hash_bytes(&blob.content).as_str() != hash)
        {
            return Ok(None);
        }
    }
    Ok(Some(VerifiedContent { base }))
}

fn raw_url(remote: &RemoteIdentity, head: &Hash, path: &str) -> Option<reqwest::Url> {
    // Reject dot components before Url can normalize them into another route.
    // Segment encoding preserves literal ?, #, %, slashes in identities, and UTF-8.
    let segments: Vec<_> = path.split('/').collect();
    if segments
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
        || [remote.owner.as_str(), remote.repo_name.as_str()]
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return None;
    }
    let mut url = reqwest::Url::parse(&remote.remote_url).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend([
            "api",
            &remote.owner,
            &remote.repo_name,
            "raw",
            head.as_str(),
        ])
        .extend(segments);
    Some(url)
}

async fn read_raw(
    client: &reqwest::Client,
    remote: &RemoteIdentity,
    head: &Hash,
    path: &str,
    hash: &Hash,
    budget: &mut ReviewPreparationBudget,
) -> Option<Vec<u8>> {
    let url = raw_url(remote, head, path)?;
    let mut request = client.get(url);
    if let Some(token) = remote.token.as_deref() {
        request = request.bearer_auth(token);
    }
    // Both response headers and every body frame share the original preparation
    // deadline. No retry, error-body reflection, or ordinary-fetch fallback.
    let response = tokio::time::timeout(budget.remaining(), request.send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > budget.bytes_left as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(frame) = tokio::time::timeout(budget.remaining(), stream.next())
        .await
        .ok()?
    {
        let frame = frame.ok()?;
        if frame.len() > budget.bytes_left {
            return None;
        }
        budget.bytes_left -= frame.len();
        bytes.extend_from_slice(&frame);
    }
    (oak_core::hash_bytes(&bytes) == *hash).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn read_request_headers(stream: &mut tokio::net::TcpStream) {
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            assert!(headers.len() < 8192);
            let mut byte = [0; 1];
            stream.read_exact(&mut byte).await.unwrap();
            headers.push(byte[0]);
        }
    }

    fn remote(url: String) -> RemoteIdentity {
        RemoteIdentity {
            remote_url: url,
            owner: "oak".into(),
            repo_name: "oak".into(),
            token: None,
        }
    }

    #[test]
    fn raw_urls_encode_segments_and_reject_route_traversal() {
        let mut remote = remote("https://oak.space".into());
        remote.owner = "a/b?#%".into();
        remote.repo_name = "répo".into();
        let hash = oak_core::hash_bytes(b"head");
        let url = raw_url(&remote, &hash, "dir/a #?%é.txt").unwrap();
        assert!(url.as_str().contains("/a%2Fb%3F%23%25/r%C3%A9po/raw/"));
        assert!(url.as_str().ends_with("/dir/a%20%23%3F%25%C3%A9.txt"));
        assert!(url.query().is_none());
        assert!(url.fragment().is_none());
        for path in ["../secret", "a/../secret", "./file", "/file", "a//b"] {
            assert!(raw_url(&remote, &hash, path).is_none());
        }
        // An encoded dot sequence in a literal filename must stay literal.
        assert!(raw_url(&remote, &hash, "%2e%2e/file")
            .unwrap()
            .path()
            .ends_with("/%252e%252e/file"));
    }

    #[tokio::test]
    async fn raw_content_uses_one_cumulative_byte_budget() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 32]))
            .mount(&server)
            .await;
        let hash = oak_core::hash_bytes(&[b'x'; 32]);
        let mut budget = ReviewPreparationBudget::new();
        budget.bytes_left = 64;
        let client = crate::http::api_client();
        for _ in 0..2 {
            assert!(read_raw(
                &client,
                &remote(server.uri()),
                &hash,
                "file",
                &hash,
                &mut budget
            )
            .await
            .is_some());
        }
        assert_eq!(budget.bytes_left, 0);
        assert!(read_raw(
            &client,
            &remote(server.uri()),
            &hash,
            "file",
            &hash,
            &mut budget
        )
        .await
        .is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn raw_stream_without_content_length_enforces_64_byte_boundary() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for size in [64, 65] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request_headers(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                stream.write_all(&vec![b'x'; size]).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let hash = oak_core::hash_bytes(&vec![b'x'; size]);
            let mut budget = ReviewPreparationBudget::new();
            budget.bytes_left = 64;
            let result = read_raw(
                &crate::http::api_client(),
                &remote(format!("http://{address}")),
                &hash,
                "file",
                &hash,
                &mut budget,
            )
            .await;
            assert_eq!(result.is_some(), size == 64);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn raw_redirect_never_forwards_credentials_or_requests_a_second_endpoint() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/private", server.uri())),
            )
            .mount(&server)
            .await;
        let mut remote = remote(server.uri());
        remote.token = Some("test-only-credential".into());
        let hash = oak_core::hash_bytes(b"x");
        assert!(read_raw(
            &crate::http::api_client(),
            &remote,
            &hash,
            "file",
            &hash,
            &mut ReviewPreparationBudget::new()
        )
        .await
        .is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn shared_deadline_bounds_headers_and_body() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for stall_headers in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request_headers(&mut stream).await;
                if !stall_headers {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\n")
                        .await
                        .unwrap();
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            });
            let hash = oak_core::hash_bytes(b"x");
            let mut budget =
                ReviewPreparationBudget::with_limits(64, std::time::Duration::from_millis(50));
            let start = std::time::Instant::now();
            assert!(read_raw(
                &crate::http::api_client(),
                &remote(format!("http://{address}")),
                &hash,
                "file",
                &hash,
                &mut budget
            )
            .await
            .is_none());
            assert!(start.elapsed() < std::time::Duration::from_secs(2));
            server.abort();
        }
    }

    #[tokio::test]
    async fn acquisition_cap_is_deduplicated_and_does_not_reject_warm_content() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for (count, cached, duplicate, expected_requests, available) in [
            (257, false, false, 0, false),
            (257, true, false, 0, true),
            (257, false, true, 1, true),
            (256, false, false, 256, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = oak_core::SqliteRepository::open(&temp.path().join("oak.db")).unwrap();
            let base = Manifest::empty();
            repo.store_manifest(&base).unwrap();
            let main = oak_core::Commit::with_timestamp(
                "main".into(),
                None,
                None,
                base.hash,
                "qa".into(),
                None,
                vec![],
                chrono::Utc::now(),
            )
            .unwrap();
            repo.store_commit(&main).unwrap();
            let entries = (0..count)
                .map(|i| {
                    let content = if duplicate { "0".into() } else { i.to_string() };
                    let hash = oak_core::hash_bytes(content.as_bytes());
                    if cached {
                        repo.put_blob(content.into_bytes()).unwrap();
                    }
                    ManifestEntry {
                        path: i.to_string(),
                        blob_hash: hash,
                        mode: FileMode::Regular,
                    }
                })
                .collect();
            let manifest = Manifest::new(entries);
            repo.store_manifest(&manifest).unwrap();
            let feature = oak_core::Commit::with_timestamp(
                "feature".into(),
                Some(main.hash.clone()),
                None,
                manifest.hash,
                "qa".into(),
                None,
                vec![],
                chrono::Utc::now(),
            )
            .unwrap();
            repo.store_commit(&feature).unwrap();
            let source = RemoteAnalysisBranch {
                name: "feature".into(),
                head: Some(feature.hash),
                parent: Some("main".into()),
            };
            let target = RemoteAnalysisBranch {
                name: "main".into(),
                head: Some(main.hash),
                parent: None,
            };
            let comparison = remote_branch_comparison(&repo, &source, &target).unwrap();
            let server = MockServer::start().await;
            if cached {
                let mut incompatible = source.clone();
                incompatible.parent = Some("another-target".into());
                assert!(prepare(
                    &repo,
                    &remote(server.uri()),
                    &incompatible,
                    &target,
                    &comparison,
                    &mut ReviewPreparationBudget::new()
                )
                .await
                .unwrap()
                .is_none());
                assert!(snapshot_content_verified(&repo, &comparison).unwrap());
                assert!(tree_diff_evidence(&repo, &comparison, "feature", "main")
                    .unwrap()
                    .changed_files
                    .iter()
                    .all(|file| file.stats_available));
            }
            Mock::given(method("GET"))
                .respond_with(move |request: &wiremock::Request| {
                    let content = if duplicate {
                        "0"
                    } else {
                        request.url.path().rsplit('/').next().unwrap()
                    };
                    ResponseTemplate::new(200).set_body_bytes(content.as_bytes())
                })
                .mount(&server)
                .await;
            let result = prepare(
                &repo,
                &remote(server.uri()),
                &source,
                &target,
                &comparison,
                &mut ReviewPreparationBudget::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.is_some(), available);
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                expected_requests
            );
        }
    }
}
