//! Working-tree evidence surfaces on `oak diff` / `oak status`:
//!
//! - `--name-only` combines with `--print` (fb-97): both are non-interactive.
//! - `oak diff --check [--json]` (fb-377): whitespace errors and leftover
//!   conflict markers on added lines, exit 1 when found, parity with
//!   `git diff --check` on the same content.
//! - `--fingerprint` / `--verify-fingerprint` (fb-376 family): a read-only
//!   working-tree identity equal to the id `oak change capture` records,
//!   computed without writing anything and without a linked remote.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(dir)
        .args(args)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("NO_COLOR", "1")
        .env_remove("OAK_API_KEY")
        .output()
        .expect("oak should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

fn ok(output: &Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    stdout(output)
}

/// Exactly one JSON document on stdout.
fn one_json(output: &Output, what: &str) -> serde_json::Value {
    let text = stdout(output);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let value = stream
        .next()
        .unwrap_or_else(|| panic!("{what}: no JSON in {text:?}"))
        .unwrap_or_else(|e| panic!("{what}: invalid JSON ({e}) in {text:?}"));
    assert!(stream.next().is_none(), "{what}: >1 JSON document: {text}");
    value
}

fn init_with(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let repo = tempfile::TempDir::new().unwrap();
    ok(&oak(repo.path(), &["init"]), "oak init");
    for (path, bytes) in files {
        let target = repo.path().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }
    ok(
        &oak(repo.path(), &["commit", "--json", "--quiet"]),
        "oak commit",
    );
    repo
}

fn head(dir: &Path) -> String {
    ok(&oak(dir, &["hash"]), "oak hash").trim().to_string()
}

// ---------------------------------------------------------------------------
// fb-97: --name-only with --print
// ---------------------------------------------------------------------------

#[test]
fn name_only_combines_with_print_for_revisions_and_worktree() {
    let repo = init_with(&[("a.txt", b"a\n")]);
    let first = head(repo.path());
    fs::write(repo.path().join("b.txt"), b"b\n").unwrap();
    fs::write(repo.path().join("a.txt"), b"A\n").unwrap();
    ok(
        &oak(repo.path(), &["commit", "--json", "--quiet"]),
        "oak commit",
    );
    let second = head(repo.path());

    let out = oak(
        repo.path(),
        &["diff", &first, &second, "--name-only", "--print"],
    );
    assert_eq!(ok(&out, "diff A B --name-only --print"), "a.txt\nb.txt\n");

    fs::write(repo.path().join("c.txt"), b"c\n").unwrap();
    let out = oak(repo.path(), &["diff", "--name-only", "--print"]);
    assert_eq!(ok(&out, "diff --name-only --print"), "c.txt\n");
}

// ---------------------------------------------------------------------------
// fb-377: oak diff --check
// ---------------------------------------------------------------------------

const CHECK_BASE: &[(&str, &[u8])] = &[
    ("a.txt", b"a\n"),
    ("clean.txt", b"fine\n"),
    ("old_ws.txt", b"pre-existing   \n"),
];

/// Every defect class, on added lines only. `old_ws.txt` keeps its
/// pre-existing trailing whitespace untouched in context: not reported.
fn dirty_check_fixture(root: &Path) {
    fs::write(
        root.join("a.txt"),
        b"a  \n<<<<<<< ours\nb\t\n  \tc\n|||||||\n=======\n>>>>>>> theirs\nok\n\n\n",
    )
    .unwrap();
    fs::write(root.join("clean.txt"), b"fine\nstill fine\n").unwrap();
    fs::write(root.join("old_ws.txt"), b"pre-existing   \nnew line\n").unwrap();
    // Not conflict markers: too short, too long, or text glued to
    // `=======`. `======= ` / `=======\t` (whitespace after) ARE markers,
    // as git flags them.
    fs::write(
        root.join("near.txt"),
        b"<<<<<< six\n<<<<<<<<eight\n=======x\n======= sep\n=======\tsep\n",
    )
    .unwrap();
}

#[test]
fn check_reports_each_problem_kind_with_lines_and_exits_one() {
    let repo = init_with(CHECK_BASE);
    dirty_check_fixture(repo.path());

    let out = oak(repo.path(), &["diff", "--check", "--json"]);
    assert_eq!(out.status.code(), Some(1), "problems found => exit 1");
    let json = one_json(&out, "diff --check --json");
    assert_eq!(json["kind"], "diff_check");
    assert_eq!(json["clean"], false);
    let got: Vec<(String, u64, String)> = json["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["path"].as_str().unwrap().to_string(),
                p["line"].as_u64().unwrap(),
                p["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let want: Vec<(String, u64, String)> = [
        ("a.txt", 1, "trailing_whitespace"),
        ("a.txt", 2, "conflict_marker"),
        ("a.txt", 3, "trailing_whitespace"),
        ("a.txt", 4, "space_before_tab"),
        ("a.txt", 5, "conflict_marker"),
        ("a.txt", 6, "conflict_marker"),
        ("a.txt", 7, "conflict_marker"),
        ("a.txt", 9, "blank_at_eof"),
        ("near.txt", 4, "conflict_marker"),
        ("near.txt", 5, "conflict_marker"),
    ]
    .iter()
    .map(|(p, l, k)| (p.to_string(), *l, k.to_string()))
    .collect();
    assert_eq!(got, want, "{json}");
    assert_eq!(json["problem_count"], 10);

    // Human form: git's `path:line: message` shape, same exit code.
    let out = oak(repo.path(), &["diff", "--check"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(
        text.contains("a.txt:1: trailing whitespace.\n+a  \n"),
        "{text}"
    );
    assert!(
        text.contains("a.txt:2: leftover conflict marker\n"),
        "{text}"
    );
    assert!(text.contains("a.txt:9: new blank line at EOF.\n"), "{text}");

    // Path filters scope the check like any diff.
    let out = oak(
        repo.path(),
        &["diff", "--check", "--json", "--", "clean.txt"],
    );
    assert_eq!(out.status.code(), Some(0));
    let json = one_json(&out, "scoped check");
    assert_eq!(json["clean"], true);
    assert_eq!(json["files_checked"], 1);
}

/// `--check` owns the exit code (1 = problems found); combining it with
/// `--exit-code` would be ambiguous, so it is a usage error.
#[test]
fn check_conflicts_with_exit_code() {
    let repo = init_with(CHECK_BASE);
    fs::write(repo.path().join("clean.txt"), b"fine\nmore\n").unwrap();
    let out = oak(repo.path(), &["diff", "--check", "--exit-code"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

#[test]
fn check_on_a_clean_change_exits_zero() {
    let repo = init_with(CHECK_BASE);
    fs::write(repo.path().join("clean.txt"), b"fine\nmore\n").unwrap();
    let out = oak(repo.path(), &["diff", "--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert_eq!(stdout(&out), "");
    let json = one_json(&oak(repo.path(), &["diff", "--check", "--json"]), "clean");
    assert_eq!(json["clean"], true);
    assert_eq!(json["problem_count"], 0);
}

#[test]
fn check_covers_revision_endpoints() {
    let repo = init_with(CHECK_BASE);
    let base = head(repo.path());
    fs::write(repo.path().join("a.txt"), b"a\ntrailing \n").unwrap();
    ok(
        &oak(repo.path(), &["commit", "--json", "--quiet"]),
        "oak commit",
    );
    let tip = head(repo.path());
    let out = oak(repo.path(), &["diff", &base, &tip, "--check", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let json = one_json(&out, "endpoint check");
    assert_eq!(json["problems"][0]["path"], "a.txt");
    assert_eq!(json["problems"][0]["line"], 2);
    assert_eq!(json["problems"][0]["kind"], "trailing_whitespace");
}

/// The `path:line: message` lines match `git diff --check` on the same
/// base and working tree (git's default whitespace rules; LF content).
#[test]
fn check_matches_git_diff_check() {
    let git_ok = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !git_ok {
        eprintln!("skipping: git is not available as the parity oracle");
        return;
    }
    let repo = init_with(CHECK_BASE);
    dirty_check_fixture(repo.path());
    let oak_lines: Vec<String> = stdout(&oak(repo.path(), &["diff", "--check"]))
        .lines()
        .filter(|l| !l.starts_with('+'))
        .map(str::to_string)
        .collect();

    let fixture = tempfile::TempDir::new().unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(fixture.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", fixture.path().join("no-global"))
            .env("GIT_CEILING_DIRECTORIES", fixture.path())
            .args(args)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    for (path, bytes) in CHECK_BASE {
        fs::write(fixture.path().join(path), bytes).unwrap();
    }
    assert!(git(&["add", "-A"]).status.success());
    assert!(git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "-q",
        "-m",
        "base"
    ])
    .status
    .success());
    dirty_check_fixture(fixture.path());
    assert!(git(&["add", "-N", "near.txt"]).status.success());
    let out = git(&["diff", "--check"]);
    assert_ne!(out.status.code(), Some(0), "git found problems too");
    let git_lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.starts_with('+'))
        .map(str::to_string)
        .collect();
    assert!(!git_lines.is_empty(), "oracle reported nothing");
    assert_eq!(oak_lines, git_lines);
}

// ---------------------------------------------------------------------------
// fb-376: --fingerprint / --verify-fingerprint
// ---------------------------------------------------------------------------

fn fingerprint(dir: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["diff", "--json", "--fingerprint"];
    args.extend_from_slice(extra);
    let out = oak(dir, &args);
    let json = one_json(&out, "diff --json --fingerprint");
    assert!(out.status.success(), "{json}");
    // The ordinary diff document is intact, with the fingerprint appended.
    assert!(json["changed_files"].is_array(), "{json}");
    json["fingerprint"].clone()
}

fn db(dir: &Path) -> SqliteRepository {
    SqliteRepository::open(&dir.join(".oak/oak.db")).unwrap()
}

fn row_count(dir: &Path, table: &str) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(
        dir.join(".oak/oak.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn fingerprint_works_unlinked_and_writes_nothing() {
    let repo = init_with(&[("a.txt", b"a\n"), ("keep.txt", b"k\n")]);
    fs::write(repo.path().join("a.txt"), b"fingerprint-only bytes\n").unwrap();
    fs::write(repo.path().join("new.txt"), b"brand new bytes\n").unwrap();
    let before_captures = row_count(repo.path(), "change_captures");
    let before_sets = row_count(repo.path(), "change_sets");
    let before_cache = db(repo.path()).load_stat_cache().unwrap();

    let fp = fingerprint(repo.path(), &[]);
    assert_eq!(fp["kind"], "working_tree_fingerprint");
    assert_eq!(fp["changed_file_count"], 2);
    assert_eq!(fp["persisted"], false);
    assert_eq!(fp["remote_contacted"], false);
    assert_eq!(fp["base_commit"].as_str().unwrap(), head(repo.path()));
    assert_eq!(fp["change_id"].as_str().unwrap().len(), 64);

    // Nothing written: no capture rows, no new blobs, no result tree, no
    // stat-cache rows.
    let repo_db = db(repo.path());
    assert_eq!(row_count(repo.path(), "change_captures"), before_captures);
    assert_eq!(row_count(repo.path(), "change_sets"), before_sets);
    for bytes in [&b"fingerprint-only bytes\n"[..], b"brand new bytes\n"] {
        assert!(
            repo_db
                .get_blob(&oak_core::hash_bytes(bytes))
                .unwrap()
                .is_none(),
            "fingerprint stored a blob"
        );
    }
    let result_tree = oak_core::Hash::from_hex(fp["result_tree"].as_str().unwrap()).unwrap();
    assert!(repo_db.get_tree(&result_tree).unwrap().is_none());
    assert_eq!(repo_db.load_stat_cache().unwrap(), before_cache);

    // Deterministic, and identical from `oak status --json --fingerprint`.
    assert_eq!(fingerprint(repo.path(), &[])["change_id"], fp["change_id"]);
    let status = one_json(
        &oak(repo.path(), &["status", "--json", "--fingerprint"]),
        "status --json --fingerprint",
    );
    assert_eq!(status["fingerprint"]["change_id"], fp["change_id"]);
    assert!(status["changes"].is_array(), "status document intact");
}

#[test]
fn fingerprint_equals_change_capture_id_full_and_scoped() {
    let repo = init_with(&[("a.txt", b"a\n"), ("dir/b.txt", b"b\n")]);
    fs::write(repo.path().join("a.txt"), b"changed a\n").unwrap();
    fs::write(repo.path().join("dir/b.txt"), b"changed b\n").unwrap();
    fs::remove_file(repo.path().join("dir/b.txt")).unwrap();
    fs::write(repo.path().join("dir/c.txt"), b"c\n").unwrap();

    // Fingerprint first (unlinked), then link identity so capture can run.
    let full = fingerprint(repo.path(), &[]);
    let scoped = fingerprint(repo.path(), &["--", "dir"]);
    assert_ne!(full["change_id"], scoped["change_id"]);
    assert_eq!(scoped["changed_file_count"], 2);
    {
        let repo_db = db(repo.path());
        repo_db
            .set_metadata(MetadataKey::RepoOwner, "acme")
            .unwrap();
        repo_db
            .set_metadata(MetadataKey::RepoName, "widgets")
            .unwrap();
    }
    let capture = |paths: &[&str]| {
        let mut args = vec!["change", "capture", "--json"];
        args.extend_from_slice(paths);
        let out = oak(repo.path(), &args);
        let json = one_json(&out, "change capture");
        assert!(out.status.success(), "{json}");
        json
    };
    let captured = capture(&[]);
    assert_eq!(captured["change_set"]["id"], full["change_id"]);
    assert_eq!(captured["base"]["tree"], full["base_tree"]);
    assert_eq!(captured["base"]["commit"], full["base_commit"]);
    assert_eq!(captured["change_set"]["result_tree"], full["result_tree"]);
    assert_eq!(
        captured["change_set"]["change_count"],
        full["changed_file_count"]
    );
    assert_eq!(capture(&["dir"])["change_set"]["id"], scoped["change_id"]);

    // And the fingerprint is unchanged by the capture having happened.
    assert_eq!(
        fingerprint(repo.path(), &[])["change_id"],
        full["change_id"]
    );
}

#[test]
fn fingerprint_changes_on_one_byte_and_mode_edits() {
    let repo = init_with(&[("run.sh", b"echo hi\n")]);
    fs::write(repo.path().join("run.sh"), b"echo ho\n").unwrap();
    let a = fingerprint(repo.path(), &[])["change_id"].clone();
    fs::write(repo.path().join("run.sh"), b"echo hO\n").unwrap();
    let b = fingerprint(repo.path(), &[])["change_id"].clone();
    assert_ne!(a, b, "one-byte edit changes the id");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            repo.path().join("run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let c = fingerprint(repo.path(), &[])["change_id"].clone();
        assert_ne!(b, c, "mode change changes the id");
    }
}

#[test]
fn verify_fingerprint_exits_zero_on_match_and_one_on_mismatch() {
    let repo = init_with(&[("a.txt", b"a\n")]);
    fs::write(repo.path().join("a.txt"), b"b\n").unwrap();
    let id = fingerprint(repo.path(), &[])["change_id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = oak(repo.path(), &["diff", "--verify-fingerprint", &id]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let out = oak(
        repo.path(),
        &["diff", "--verify-fingerprint", &id, "--json"],
    );
    assert_eq!(out.status.code(), Some(0));
    let json = one_json(&out, "verify --json");
    assert_eq!(json["matches"], true);

    fs::write(repo.path().join("a.txt"), b"c\n").unwrap();
    let out = oak(
        repo.path(),
        &["diff", "--verify-fingerprint", &id, "--json"],
    );
    assert_eq!(out.status.code(), Some(1), "mismatch => exit 1");
    let json = one_json(&out, "verify mismatch");
    assert_eq!(json["matches"], false);
    assert_eq!(json["expected_change_id"], id.as_str());
    assert_ne!(json["fingerprint"]["change_id"], id.as_str());
    let out = oak(repo.path(), &["diff", "--verify-fingerprint", &id]);
    assert_eq!(out.status.code(), Some(1));

    // A malformed id is a usage problem, never a silent match.
    let out = oak(repo.path(), &["diff", "--verify-fingerprint", "abc123"]);
    assert!(!out.status.success());
    assert_ne!(out.status.code(), Some(1));
}
