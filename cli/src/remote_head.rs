//! Remote head observation: the one place that answers "what is the remote
//! `main` head, and how do we know?".
//!
//! Local `main` is a copy of a server-owned ref. Every command that acts on
//! it — creating a branch "from latest main", certifying a merge prediction,
//! reporting freshness — needs the same three facts: the head, when it was
//! last verified against the remote, and whether *this invocation* verified
//! it. Before this module those facts were derived independently (two private
//! 60 s TTL constants, a timestamp stored without the head it described), so
//! a recent timestamp could vouch for a `main` the remote had already moved
//! past (fb-627, fb-629, fb-117).
//!
//! Rules enforced here:
//!
//! * A verified observation is persisted as head **and** timestamp together
//!   ([`MetadataKey::MainLastObservedHead`]). A cached observation is valid
//!   only while the stored head is still the local `main` head.
//! * [`RemoteHeadSource::Cached`] is never the same thing as
//!   [`RemoteHeadSource::VerifiedRemote`]: callers that certify or promise
//!   "latest" use [`ObservePolicy::MustVerify`] and get one head GET per
//!   invocation.
//! * A failed verification is reported ([`RemoteHeadSource::Unverified`]),
//!   never papered over with the cached value.

use std::time::{SystemTime, UNIX_EPOCH};

use oak_core::{Hash, MetadataKey, Repository, Result, DEFAULT_BRANCH};

use crate::commands::sync::{self, AncestryNeed};

/// How long a persisted observation counts as *recent*. The only freshness
/// window in the CLI, and it only chooses a label: merge safety reports a
/// cached target as `fresh_local` inside it and `stale_local` outside it,
/// uncertified either way. No policy skips verification because of it.
pub const REMOTE_HEAD_CACHE_TTL_SECS: u64 = 60;

/// How an observation was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteHeadSource {
    /// The remote was asked for the head in this invocation, and local `main`
    /// now is that head.
    VerifiedRemote,
    /// No remote round-trip in this invocation; an earlier verified
    /// observation of exactly this head is on record, `age_secs` old.
    Cached { age_secs: u64 },
    /// No remote round-trip and no usable earlier observation (never
    /// verified, verified for a different head, or no remote configured).
    LocalOnly,
    /// Verification was required and attempted, and failed. The head is the
    /// local one.
    Unverified { error: String },
}

impl RemoteHeadSource {
    /// Machine-contract value (receipts: `base_head_source`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VerifiedRemote => "verified_remote",
            Self::Cached { .. } => "cached",
            Self::LocalOnly => "local_only",
            Self::Unverified { .. } => "unverified",
        }
    }
}

/// One observation of the remote `main` head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHeadObservation {
    pub branch: String,
    /// The local `main` head the observation describes. For
    /// [`RemoteHeadSource::VerifiedRemote`] it is also the remote head.
    pub head: Option<Hash>,
    /// Unix seconds of the last successful verification of `head` against
    /// the remote; `None` when this head was never verified.
    pub observed_at: Option<u64>,
    pub source: RemoteHeadSource,
}

impl RemoteHeadObservation {
    /// Verified against the remote by this invocation. The only state that
    /// may certify a merge prediction or back a "from latest main" claim.
    pub fn is_verified(&self) -> bool {
        match self.source {
            RemoteHeadSource::VerifiedRemote => true,
            RemoteHeadSource::Cached { .. }
            | RemoteHeadSource::LocalOnly
            | RemoteHeadSource::Unverified { .. } => false,
        }
    }

    /// A cached observation inside [`REMOTE_HEAD_CACHE_TTL_SECS`].
    pub fn is_recent_cache(&self) -> bool {
        match self.source {
            RemoteHeadSource::Cached { age_secs } => age_secs <= REMOTE_HEAD_CACHE_TTL_SECS,
            RemoteHeadSource::VerifiedRemote
            | RemoteHeadSource::LocalOnly
            | RemoteHeadSource::Unverified { .. } => false,
        }
    }

    /// `observed_at` as RFC 3339 (UTC), for receipts.
    pub fn observed_at_rfc3339(&self) -> Option<String> {
        self.observed_at.and_then(rfc3339)
    }

    /// One sentence for human output: how much the head can be trusted.
    pub fn describe(&self) -> String {
        match &self.source {
            RemoteHeadSource::VerifiedRemote => "verified against the remote just now".to_string(),
            RemoteHeadSource::Cached { age_secs } => {
                format!("last verified against the remote {age_secs}s ago")
            }
            RemoteHeadSource::LocalOnly => "never verified against the remote".to_string(),
            RemoteHeadSource::Unverified { error } => match self.observed_at {
                Some(at) => format!(
                    "could not be verified against the remote ({error}); last verified {}s ago",
                    unix_now().saturating_sub(at)
                ),
                None => {
                    format!("could not be verified against the remote ({error}); never verified")
                }
            },
        }
    }
}

/// Unix seconds as RFC 3339 (UTC).
pub fn rfc3339(unix_secs: u64) -> Option<String> {
    let secs = i64::try_from(unix_secs).ok()?;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// What a caller needs from an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservePolicy {
    /// Ask the remote in this invocation (one head GET; `main`'s objects are
    /// fetched only when the head moved). Used by anything that promises
    /// "latest main" or certifies against it.
    MustVerify,
    /// No network. Reports what is on record.
    Offline,
}

/// Network-leg options for [`observe`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ObserveOptions<'a> {
    /// Use this remote instead of the stored one.
    pub remote_override: Option<&'a str>,
    /// Route the "Updating 'main'…" narration to the verbose log.
    pub quiet: bool,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn remote_configured(repo: &dyn Repository) -> Result<bool> {
    Ok(repo
        .get_metadata(MetadataKey::RemoteUrl)?
        .is_some_and(|remote| !remote.trim().is_empty()))
}

/// Persist a verified observation of the remote `main` head: the head and the
/// time, in one value, after a fetch that left local `main` at that head.
///
/// Compat shim: `MainLastCheckedAt` (timestamp only) is still written here —
/// and only here, the same fetch path that always wrote it — because
/// released binaries read it as "local main is fresh". Nothing in this
/// binary reads it.
pub fn record_verified(repo: &dyn Repository, head: Option<&Hash>) {
    let now = unix_now();
    store_observation(repo, now, head);
    let _ = repo.set_metadata(MetadataKey::MainLastCheckedAt, &now.to_string());
}

/// Persist a sighting of the remote `main` head by something that does not
/// fetch it (the watch daemon's poll). The head may be ahead of local
/// `main`; such a record simply does not validate for local `main` until a
/// fetch catches up — which is the truth. The legacy timestamp is not
/// touched: this binary never tells an older one that a `main` it did not
/// fetch is fresh.
pub fn record_seen(repo: &dyn Repository, head: Option<&Hash>) {
    store_observation(repo, unix_now(), head);
}

fn store_observation(repo: &dyn Repository, at: u64, head: Option<&Hash>) {
    let _ = repo.set_metadata(
        MetadataKey::MainLastObservedHead,
        &format!("{at} {}", head.map(Hash::as_str).unwrap_or_default()),
    );
}

/// The persisted observation, if it still describes `local_head`.
fn stored_observation_time(
    repo: &dyn Repository,
    local_head: Option<&Hash>,
) -> Result<Option<u64>> {
    // An integrity failure on the last fetch means local `main` evidence is
    // not trustworthy at all until a fetch succeeds.
    if repo
        .get_metadata(MetadataKey::MainLastFetchIntegrityError)?
        .is_some_and(|error| !error.is_empty())
    {
        return Ok(None);
    }
    let Some(raw) = repo.get_metadata(MetadataKey::MainLastObservedHead)? else {
        return Ok(None);
    };
    let (at, head) = raw.split_once(' ').unwrap_or((raw.as_str(), ""));
    let Ok(at) = at.parse::<u64>() else {
        return Ok(None);
    };
    if at == 0 || at > unix_now() {
        return Ok(None);
    }
    if head != local_head.map(Hash::as_str).unwrap_or_default() {
        return Ok(None);
    }
    Ok(Some(at))
}

/// What is on record for `main`, with no network I/O
/// ([`ObservePolicy::Offline`]).
pub fn on_record(repo: &dyn Repository) -> Result<RemoteHeadObservation> {
    let head = repo.get_branch_head(DEFAULT_BRANCH)?;
    let observed_at = if remote_configured(repo)? {
        stored_observation_time(repo, head.as_ref())?
    } else {
        None
    };
    Ok(RemoteHeadObservation {
        branch: DEFAULT_BRANCH.to_string(),
        head,
        observed_at,
        source: match observed_at {
            Some(at) => RemoteHeadSource::Cached {
                age_secs: unix_now().saturating_sub(at),
            },
            None => RemoteHeadSource::LocalOnly,
        },
    })
}

/// Observe the remote `main` head under `policy`.
///
/// Network failures are part of the answer ([`RemoteHeadSource::Unverified`]),
/// not an `Err`: the caller decides what an unverified head permits. `Err` is
/// a local storage failure.
///
/// On a verified observation local `main` *is* the remote head: when the head
/// moved, its commit, trees, and missing blobs are fetched and the local ref
/// advances (no working-tree change), exactly as `oak fetch` does.
pub async fn observe(
    repo: &dyn Repository,
    policy: ObservePolicy,
    options: ObserveOptions<'_>,
) -> Result<RemoteHeadObservation> {
    let recorded = on_record(repo)?;
    let verify = match policy {
        ObservePolicy::MustVerify => true,
        ObservePolicy::Offline => false,
    };
    if !verify || (options.remote_override.is_none() && !remote_configured(repo)?) {
        return Ok(recorded);
    }
    match sync::fetch_parent_with_bounds(
        repo,
        DEFAULT_BRANCH,
        options.remote_override,
        options.quiet,
        AncestryNeed::HeadOnly,
        sync::FetchBounds::HeadProbe,
    )
    .await
    {
        Ok(head) => Ok(RemoteHeadObservation {
            branch: DEFAULT_BRANCH.to_string(),
            head,
            observed_at: Some(unix_now()),
            source: RemoteHeadSource::VerifiedRemote,
        }),
        Err(error) => {
            sync::record_parent_integrity_failure_if_any(repo, DEFAULT_BRANCH, &error);
            // Re-read: a failed fetch leaves the local ref where it was, but
            // an integrity failure just invalidated the record.
            let recorded = on_record(repo)?;
            Ok(RemoteHeadObservation {
                source: RemoteHeadSource::Unverified {
                    error: error.to_string(),
                },
                ..recorded
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oak_core::{Branch, SqliteRepository};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn hash(byte: &str) -> Hash {
        Hash(byte.repeat(32))
    }

    /// A repository that keeps its temp directory alive.
    struct TestRepo {
        repo: SqliteRepository,
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestRepo {
        type Target = SqliteRepository;
        fn deref(&self) -> &SqliteRepository {
            &self.repo
        }
    }

    fn empty_repo() -> TestRepo {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = SqliteRepository::open(&dir.path().join("oak.db")).unwrap();
        TestRepo { repo, _dir: dir }
    }

    /// Store a `main` commit and return its hash (branch heads must name
    /// stored commits).
    fn main_commit(repo: &SqliteRepository, tag: &str) -> Hash {
        let commit = oak_core::Commit::new(
            DEFAULT_BRANCH.to_string(),
            None,
            None,
            hash("cc"),
            "tester".to_string(),
            Some(tag.to_string()),
            Vec::new(),
        )
        .unwrap();
        repo.store_commit(&commit).unwrap();
        commit.hash
    }

    /// A repo with a `main` branch; with `with_head`, at a stored commit.
    fn repo_with_main(with_head: bool) -> (TestRepo, Option<Hash>) {
        let repo = empty_repo();
        repo.store_branch(&Branch::new(DEFAULT_BRANCH.to_string(), None, None))
            .unwrap();
        let head = with_head.then(|| {
            let head = main_commit(&repo, "head");
            repo.set_branch_head(DEFAULT_BRANCH, &head).unwrap();
            head
        });
        (repo, head)
    }

    fn link(repo: &SqliteRepository, remote: &str) {
        repo.set_metadata(MetadataKey::RemoteUrl, remote).unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "acme").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "app").unwrap();
    }

    fn store_raw(repo: &SqliteRepository, at: u64, head: &str) {
        repo.set_metadata(MetadataKey::MainLastObservedHead, &format!("{at} {head}"))
            .unwrap();
    }

    #[test]
    fn a_recorded_observation_is_cached_only_for_the_head_it_was_made_of() {
        let (repo, head) = repo_with_main(true);
        let head = head.unwrap();
        link(&repo, "http://127.0.0.1:1");

        assert_eq!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::LocalOnly
        );

        record_verified(&*repo, Some(&head));
        let seen = on_record(&*repo).unwrap();
        assert!(matches!(seen.source, RemoteHeadSource::Cached { age_secs } if age_secs <= 2));
        assert_eq!(seen.head, Some(head.clone()));
        assert!(seen.observed_at.is_some());
        assert!(seen.is_recent_cache());
        assert!(
            !seen.is_verified(),
            "a cached observation is never verified"
        );

        // Local main moves (any writer): the record no longer describes it.
        let next = main_commit(&repo, "next");
        repo.set_branch_head(DEFAULT_BRANCH, &next).unwrap();
        let moved = on_record(&*repo).unwrap();
        assert_eq!(moved.source, RemoteHeadSource::LocalOnly);
        assert_eq!(moved.observed_at, None);
    }

    #[test]
    fn a_timestamp_alone_is_not_an_observation() {
        // What released binaries write: a time, no head. It cannot vouch for
        // whatever local main happens to be.
        let (repo, _head) = repo_with_main(true);
        link(&repo, "http://127.0.0.1:1");
        repo.set_metadata(MetadataKey::MainLastCheckedAt, &unix_now().to_string())
            .unwrap();
        assert_eq!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::LocalOnly
        );
    }

    #[test]
    fn old_future_malformed_and_integrity_failed_records_are_rejected() {
        let (repo, head) = repo_with_main(true);
        let head = head.unwrap();
        link(&repo, "http://127.0.0.1:1");

        store_raw(&repo, unix_now() - 3600, head.as_str());
        let old = on_record(&*repo).unwrap();
        assert!(matches!(old.source, RemoteHeadSource::Cached { age_secs } if age_secs >= 3600));
        assert!(
            !old.is_recent_cache(),
            "outside the TTL it is cached, not recent"
        );

        store_raw(&repo, unix_now() + 3600, head.as_str());
        assert_eq!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::LocalOnly
        );

        repo.set_metadata(MetadataKey::MainLastObservedHead, "not-a-time x")
            .unwrap();
        assert_eq!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::LocalOnly
        );

        record_verified(&*repo, Some(&head));
        repo.set_metadata(MetadataKey::MainLastFetchIntegrityError, "hash mismatch")
            .unwrap();
        assert_eq!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::LocalOnly
        );
    }

    #[test]
    fn an_empty_remote_main_is_an_observation_of_no_head() {
        let (repo, _) = repo_with_main(false);
        link(&repo, "http://127.0.0.1:1");
        record_verified(&*repo, None);
        let seen = on_record(&*repo).unwrap();
        assert!(matches!(seen.source, RemoteHeadSource::Cached { .. }));
        assert_eq!(seen.head, None);
    }

    #[test]
    fn record_verified_keeps_writing_the_timestamp_released_binaries_read() {
        let (repo, head) = repo_with_main(true);
        let head = head.unwrap();
        record_verified(&*repo, Some(&head));
        let legacy = repo
            .get_metadata(MetadataKey::MainLastCheckedAt)
            .unwrap()
            .unwrap();
        assert!(legacy.parse::<u64>().is_ok());
    }

    #[test]
    fn seeing_a_moved_remote_head_does_not_vouch_for_local_main() {
        // The watch daemon's poll: the remote is at `bb`, local main behind.
        let (repo, local) = repo_with_main(true);
        let local = local.unwrap();
        link(&repo, "http://127.0.0.1:1");
        record_verified(&*repo, Some(&local));
        let before = repo.get_metadata(MetadataKey::MainLastCheckedAt).unwrap();

        record_seen(&*repo, Some(&hash("bb")));
        assert_eq!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::LocalOnly
        );
        assert_eq!(
            repo.get_metadata(MetadataKey::MainLastCheckedAt).unwrap(),
            before,
            "a sighting never touches the legacy freshness timestamp"
        );

        // Seeing the head local main is at does count.
        record_seen(&*repo, Some(&local));
        assert!(matches!(
            on_record(&*repo).unwrap().source,
            RemoteHeadSource::Cached { .. }
        ));
    }

    #[tokio::test]
    async fn no_remote_is_local_only_under_every_policy() {
        let (repo, head) = repo_with_main(true);
        let head = head.unwrap();
        for policy in [ObservePolicy::MustVerify, ObservePolicy::Offline] {
            let seen = observe(&*repo, policy, ObserveOptions::default())
                .await
                .unwrap();
            assert_eq!(seen.source, RemoteHeadSource::LocalOnly, "{policy:?}");
            assert_eq!(seen.head, Some(head.clone()));
        }
    }

    async fn server_reporting(head: Option<&Hash>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/acme/app"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "app",
                "owner": "acme",
                "head": head.map(Hash::as_str),
            })))
            .mount(&server)
            .await;
        server
    }

    async fn head_requests(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/api/acme/app")
            .count()
    }

    /// A local `main` whose head commit is fully present, so an unchanged
    /// remote head needs nothing but the head GET.
    fn hydrated_repo() -> (TestRepo, Hash) {
        let (repo, head) = repo_with_main(true);
        let head = head.unwrap();
        // Every blob of the head snapshot is local.
        repo.set_metadata(MetadataKey::MainHydratedHead, head.as_str())
            .unwrap();
        (repo, head)
    }

    #[tokio::test]
    async fn must_verify_asks_the_remote_even_inside_the_ttl() {
        crate::output::begin_capture();
        let (repo, head) = hydrated_repo();
        let server = server_reporting(Some(&head)).await;
        link(&repo, &server.uri());
        record_verified(&*repo, Some(&head)); // a 0 s old cache

        let seen = observe(&*repo, ObservePolicy::MustVerify, ObserveOptions::default())
            .await
            .unwrap();
        let _ = crate::output::end_capture();
        assert_eq!(seen.source, RemoteHeadSource::VerifiedRemote);
        assert_eq!(seen.head, Some(head));
        assert!(seen.is_verified());
        assert_eq!(head_requests(&server).await, 1, "exactly one head GET");
    }

    #[tokio::test]
    async fn offline_never_touches_the_network() {
        let (repo, head) = hydrated_repo();
        let server = server_reporting(Some(&head)).await;
        link(&repo, &server.uri());
        let seen = observe(&*repo, ObservePolicy::Offline, ObserveOptions::default())
            .await
            .unwrap();
        assert_eq!(seen.source, RemoteHeadSource::LocalOnly);
        assert_eq!(head_requests(&server).await, 0);
    }

    #[tokio::test]
    async fn an_unreachable_remote_is_reported_not_replaced_by_the_cache() {
        crate::output::begin_capture();
        let (repo, head) = hydrated_repo();
        // Port 1 refuses connections.
        link(&repo, "http://127.0.0.1:1");
        record_verified(&*repo, Some(&head));

        let seen = observe(&*repo, ObservePolicy::MustVerify, ObserveOptions::default())
            .await
            .unwrap();
        let _ = crate::output::end_capture();
        assert!(
            matches!(seen.source, RemoteHeadSource::Unverified { .. }),
            "{:?}",
            seen.source
        );
        assert!(!seen.is_verified());
        assert_eq!(seen.head, Some(head), "falls back to the local head");
        assert!(
            seen.observed_at.is_some(),
            "keeps when it was last verified"
        );
        assert_eq!(seen.source.as_str(), "unverified");
    }

    #[test]
    fn rfc3339_is_utc_seconds() {
        assert_eq!(rfc3339(0).as_deref(), Some("1970-01-01T00:00:00Z"));
    }
}
