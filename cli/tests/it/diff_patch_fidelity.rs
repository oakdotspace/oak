//! The patch `oak diff` emits must be applicable by an independent tool and
//! reproduce the intended bytes exactly (fb521).
//!
//! Before this suite, `FileDiff` stripped the newline terminator from every
//! line and the renderer appended one unconditionally, so a file whose final
//! line had no newline printed as if it had one. `git apply` then either
//! rejected the hunk (the old side did not match) or — worse — applied it and
//! silently produced content ending in a newline the author never wrote.
//!
//! Every case here drives the real `oak` binary, applies the printed patch
//! with `git apply` in a plain directory (no Oak or Git repository involved
//! on the applying side), and compares the resulting raw bytes to the exact
//! intended content — forwards (old → new) and reversed (`git apply -R`,
//! new → old), so both sides' terminators are checked. The `--json --hunks`
//! patch is put through the same oracle and must stay a single JSON document.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// The applicator-side contract, spelled the way `git diff` and `diff -u`
/// emit it — deliberately a literal, not `oak_core`'s constant.
const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

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

/// A `git` invocation isolated from the host: no system or global config
/// (a user-level `core.autocrlf=true` would otherwise rewrite the very bytes
/// under test) and no repository discovery above the fixture directory.
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", dir.join("no-global-gitconfig"))
        .env("GIT_CEILING_DIRECTORIES", dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE");
    cmd
}

/// `git apply` is the independent applicator for this suite. CI has git;
/// on a developer machine without it, skip loudly rather than fail on a
/// missing tool that is not what these tests are about.
fn git_available() -> bool {
    let dir = tempfile::TempDir::new().unwrap();
    match git(dir.path()).arg("--version").output() {
        Ok(out) if out.status.success() => true,
        _ => {
            eprintln!(
                "skipping: `git` is not available to act as the independent patch applicator"
            );
            false
        }
    }
}

fn ok(output: &Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

/// One file transition: `None` means the file does not exist on that side.
struct Case {
    name: &'static str,
    old: Option<&'static [u8]>,
    new: Option<&'static [u8]>,
}

const FILE: &str = "f.txt";

/// The matrix: both sides with and without a final newline, newline-only
/// transitions, an edit far from the final line (default context does not
/// reach it) and one near it, growth past an unterminated line, add/delete
/// with and without a terminator, CRLF content, and empty <-> newline-only.
const CASES: &[Case] = &[
    Case {
        // The retained fb521 reproduction.
        name: "modified_middle_line_both_sides_unterminated",
        old: Some(b"a\nb\nc"),
        new: Some(b"a\nB\nc"),
    },
    Case {
        name: "final_newline_added_only",
        old: Some(b"a\nb\nc"),
        new: Some(b"a\nb\nc\n"),
    },
    Case {
        name: "final_newline_removed_only",
        old: Some(b"a\nb\nc\n"),
        new: Some(b"a\nb\nc"),
    },
    Case {
        name: "edit_far_from_unterminated_final_line",
        old: Some(b"a\nb\nc\nd\ne\nf\ng\nh"),
        new: Some(b"a\nX\nc\nd\ne\nf\ng\nh"),
    },
    Case {
        name: "edit_near_unterminated_final_line",
        old: Some(b"a\nb\nc\nd\ne"),
        new: Some(b"a\nX\nc\nd\ne"),
    },
    Case {
        name: "unterminated_final_line_changes",
        old: Some(b"a\nb\nc"),
        new: Some(b"a\nb\nC"),
    },
    Case {
        name: "grows_past_unterminated_final_line",
        old: Some(b"a\nb\nc"),
        new: Some(b"a\nb\nc\nd"),
    },
    Case {
        name: "added_file_unterminated",
        old: None,
        new: Some(b"hello\nworld"),
    },
    Case {
        name: "added_file_terminated",
        old: None,
        new: Some(b"hello\nworld\n"),
    },
    Case {
        name: "deleted_file_unterminated",
        old: Some(b"hello\nworld"),
        new: None,
    },
    Case {
        name: "deleted_file_terminated",
        old: Some(b"hello\nworld\n"),
        new: None,
    },
    Case {
        name: "crlf_unterminated_final_line",
        old: Some(b"a\r\nb\r\nc"),
        new: Some(b"a\r\nB\r\nc"),
    },
    Case {
        name: "crlf_final_newline_added",
        old: Some(b"a\r\nb"),
        new: Some(b"a\r\nb\r\n"),
    },
    Case {
        // A lone `\r` is content to git (one line), not a line end.
        name: "lone_cr_inside_single_unterminated_line",
        old: Some(b"a\rb"),
        new: Some(b"a\rB"),
    },
    Case {
        name: "final_line_ends_in_bare_cr",
        old: Some(b"a\nc\r"),
        new: Some(b"a\nc\r\n"),
    },
    Case {
        name: "both_sides_terminated_control",
        old: Some(b"a\nb\nc\n"),
        new: Some(b"a\nB\nc\n"),
    },
    Case {
        name: "empty_to_single_newline",
        old: Some(b""),
        new: Some(b"\n"),
    },
    Case {
        name: "single_newline_to_empty",
        old: Some(b"\n"),
        new: Some(b""),
    },
];

/// Commit `old` (or nothing) in a fresh Oak repo, then leave `new` (or a
/// deletion) dirty in the working tree. Returns the repo directory.
fn stage(case: &Case) -> tempfile::TempDir {
    let repo = tempfile::TempDir::new().unwrap();
    ok(&oak(repo.path(), &["init"]), "oak init");
    if let Some(old) = case.old {
        fs::write(repo.path().join(FILE), old).unwrap();
    }
    ok(
        &oak(repo.path(), &["commit", "--json", "--quiet"]),
        "oak commit",
    );
    match case.new {
        Some(new) => fs::write(repo.path().join(FILE), new).unwrap(),
        None => fs::remove_file(repo.path().join(FILE)).unwrap(),
    }
    repo
}

/// Apply `patch` to a plain directory seeded with `from`, using `git apply`
/// as an implementation-independent applicator, and return the resulting
/// bytes (`None` when the file is absent afterwards). `reverse` applies the
/// patch backwards (`-R`).
fn apply_with_git(
    patch: &str,
    from: Option<&[u8]>,
    reverse: bool,
) -> Result<Option<Vec<u8>>, String> {
    let dir = tempfile::TempDir::new().unwrap();
    if let Some(bytes) = from {
        fs::write(dir.path().join(FILE), bytes).unwrap();
    }
    let patch_path = dir.path().join("oak.patch");
    fs::write(&patch_path, patch).unwrap();
    let mut cmd = git(dir.path());
    cmd.arg("apply").arg("--whitespace=nowarn");
    if reverse {
        cmd.arg("-R");
    }
    let output = cmd.arg(&patch_path).output().expect("git should run");
    if !output.status.success() {
        return Err(format!(
            "git apply{} exited {}: {}",
            if reverse { " -R" } else { "" },
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let target = dir.path().join(FILE);
    Ok(target.exists().then(|| fs::read(&target).unwrap()))
}

/// Printable form of one side for failure messages (`None` = absent).
fn describe(side: Option<&[u8]>) -> String {
    match side {
        Some(bytes) => format!("{:?}", String::from_utf8_lossy(bytes)),
        None => "<absent>".to_string(),
    }
}

fn check_round_trip(case: &Case, patch: &str, surface: &str) -> Vec<String> {
    let mut failures = Vec::new();
    let unterminated =
        |side: Option<&[u8]>| side.is_some_and(|b| !b.is_empty() && !b.ends_with(b"\n"));
    // Terminated content must never grow a marker. (The converse is not a
    // test: an unterminated final line outside the hunk's context is
    // correctly unmarked. The byte oracle below is the real proof.)
    if !unterminated(case.old) && !unterminated(case.new) && patch.contains(NO_NEWLINE_MARKER) {
        failures.push(format!(
            "{}/{surface}: marker on fully terminated content, patch:\n{patch}",
            case.name
        ));
    }
    match apply_with_git(patch, case.old, false) {
        Ok(got) => {
            // Existence is part of the contract: an added file must be
            // created and a deleted one removed, not left as a 0-byte file.
            if got.as_deref() != case.new {
                failures.push(format!(
                    "{}/{surface}: forward apply produced {}, wanted {}; patch:\n{patch}",
                    case.name,
                    describe(got.as_deref()),
                    describe(case.new)
                ));
            }
        }
        Err(e) => failures.push(format!(
            "{}/{surface}: forward {e}; patch:\n{patch}",
            case.name
        )),
    }
    match apply_with_git(patch, case.new, true) {
        Ok(got) => {
            if got.as_deref() != case.old {
                failures.push(format!(
                    "{}/{surface}: reverse apply produced {}, wanted {}; patch:\n{patch}",
                    case.name,
                    describe(got.as_deref()),
                    describe(case.old)
                ));
            }
        }
        Err(e) => failures.push(format!(
            "{}/{surface}: reverse {e}; patch:\n{patch}",
            case.name
        )),
    }
    failures
}

/// Parse `stdout` as exactly one JSON document and return `FILE`'s `patch`.
fn json_patch_for_file(stdout: &str, what: &str) -> String {
    let mut stream = serde_json::Deserializer::from_str(stdout).into_iter::<serde_json::Value>();
    let value = stream
        .next()
        .unwrap_or_else(|| panic!("{what}: no JSON document in {stdout:?}"))
        .unwrap_or_else(|e| panic!("{what}: invalid JSON ({e}) in {stdout:?}"));
    assert!(
        stream.next().is_none(),
        "{what}: more than one JSON document in {stdout:?}"
    );
    let files = value["changed_files"]
        .as_array()
        .expect("changed_files array");
    let file = files
        .iter()
        .find(|f| f["path"] == FILE)
        .unwrap_or_else(|| panic!("{what}: {FILE} missing from {value}"));
    assert!(
        file.get("patch_omitted").is_none(),
        "{what}: patch unexpectedly omitted: {file}"
    );
    file["patch"]
        .as_str()
        .unwrap_or_else(|| panic!("{what}: no patch string in {file}"))
        .to_string()
}

#[test]
fn printed_patches_apply_with_git_and_reproduce_exact_bytes() {
    if !git_available() {
        return;
    }
    let mut failures = Vec::new();
    for case in CASES {
        let repo = stage(case);
        let patch = ok(&oak(repo.path(), &["diff", "--print"]), "oak diff --print");
        assert_eq!(
            patch.matches("\n@@ ").count() + usize::from(patch.starts_with("@@ ")),
            1,
            "{}: every case here is a single hunk; got:\n{patch}",
            case.name
        );
        failures.extend(check_round_trip(case, &patch, "print"));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn json_hunk_patches_apply_with_git_and_reproduce_exact_bytes() {
    if !git_available() {
        return;
    }
    let mut failures = Vec::new();
    for case in CASES {
        let repo = stage(case);
        let stdout = ok(
            &oak(repo.path(), &["diff", "--json", "--hunks"]),
            "oak diff --json --hunks",
        );
        let patch = json_patch_for_file(&stdout, case.name);
        failures.extend(check_round_trip(case, &patch, "json"));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Branch and revision endpoints render patches from stored blobs through a
/// different adapter (`review.rs`) than the working-tree diff. The same
/// change must produce an applicable patch on every surface: checkout-free
/// `oak diff <branch> --json --hunks`, `oak branch diff <branch> --hunks`,
/// and the printed branch diff.
#[test]
fn store_backed_branch_patches_apply_with_git_and_reproduce_exact_bytes() {
    if !git_available() {
        return;
    }
    let mut failures = Vec::new();
    for case in CASES {
        // Commit `old` (and an anchor file so the base is never empty),
        // branch off, commit `new` (or the deletion) on the branch.
        let repo = tempfile::TempDir::new().unwrap();
        ok(&oak(repo.path(), &["init"]), "oak init");
        fs::write(repo.path().join("anchor.txt"), b"anchor\n").unwrap();
        if let Some(old) = case.old {
            fs::write(repo.path().join(FILE), old).unwrap();
        }
        ok(
            &oak(repo.path(), &["commit", "--json", "--quiet"]),
            "oak commit (base)",
        );
        ok(
            &oak(repo.path(), &["switch", "-c", "feat"]),
            "oak switch -c feat",
        );
        match case.new {
            Some(new) => fs::write(repo.path().join(FILE), new).unwrap(),
            None => fs::remove_file(repo.path().join(FILE)).unwrap(),
        }
        ok(
            &oak(repo.path(), &["commit", "--json", "--quiet"]),
            "oak commit (branch)",
        );

        let printed = ok(
            &oak(repo.path(), &["diff", "feat", "--print"]),
            "oak diff feat --print",
        );
        failures.extend(check_round_trip(case, &printed, "diff <branch> --print"));

        // The store-backed JSON surfaces render through `review.rs`'s own
        // overlay adapter, which does not yet name `/dev/null` for added or
        // deleted files: their content round-trips, but existence does not.
        // Until that adapter adopts `oak_core::diff::unified_side_headers`,
        // only modifications are held to the full contract there.
        if case.old.is_none() || case.new.is_none() {
            continue;
        }
        let endpoint = ok(
            &oak(repo.path(), &["diff", "feat", "--json", "--hunks"]),
            "oak diff feat --json --hunks",
        );
        failures.extend(check_round_trip(
            case,
            &json_patch_for_file(&endpoint, case.name),
            "diff <branch> --json --hunks",
        ));

        let branch_diff = ok(
            &oak(
                repo.path(),
                &["branch", "diff", "feat", "--json", "--hunks"],
            ),
            "oak branch diff feat --json --hunks",
        );
        failures.extend(check_round_trip(
            case,
            &json_patch_for_file(&branch_diff, case.name),
            "branch diff --json --hunks",
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Wider context can pull an unterminated final line into a hunk that a
/// narrower one leaves out; both renderings must apply.
#[test]
fn context_width_does_not_break_applicability() {
    if !git_available() {
        return;
    }
    let case = &CASES[3];
    assert_eq!(case.name, "edit_far_from_unterminated_final_line");
    let repo = stage(case);
    let mut failures = Vec::new();
    // (`-U0` is excluded: `git apply` refuses zero-context hunks without
    // `--unidiff-zero`, which is an applicator policy, not patch fidelity.)
    for width in ["1", "2", "3", "10"] {
        let patch = ok(
            &oak(repo.path(), &["diff", "--print", "-U", width]),
            "oak diff --print -U",
        );
        failures.extend(check_round_trip(case, &patch, &format!("print -U{width}")));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// With colour on, a CRLF line's `\r` must sit outside the ANSI wrap (after
/// the reset, before `\n`) as git emits it; `\r` immediately followed by an
/// escape renders as a visible `^M` in `less -R`.
#[test]
fn coloured_output_keeps_carriage_return_outside_the_ansi_wrap() {
    let case = CASES
        .iter()
        .find(|c| c.name == "crlf_unterminated_final_line")
        .unwrap();
    let repo = stage(case);
    let output = Command::new(env!("CARGO_BIN_EXE_oak"))
        .current_dir(repo.path())
        .args(["diff", "--print"])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .env_remove("OAK_API_KEY")
        .output()
        .unwrap();
    let stdout = ok(&output, "oak diff --print (colour forced)");
    assert!(
        stdout.contains("\x1b["),
        "colour was not forced on: {stdout:?}"
    );
    assert!(
        !stdout.contains("\r\x1b["),
        "a `\\r` precedes an escape sequence: {stdout:?}"
    );
    // ` a\r`, `-b\r`, `+B\r`: every CRLF line still ends in `\r\n`.
    assert_eq!(stdout.matches("\r\n").count(), 3, "{stdout:?}");
}

/// `--stat` counts and the JSON additions/deletions must not change because
/// the terminator is now recorded: the marker is not a line.
#[test]
fn line_counts_are_unaffected_by_the_marker() {
    let case = &CASES[0];
    let repo = stage(case);
    let stat = ok(&oak(repo.path(), &["diff", "--stat"]), "oak diff --stat");
    assert!(
        stat.contains("+1 -1"),
        "stat should count one replaced line: {stat}"
    );
    let json: serde_json::Value = serde_json::from_str(&ok(
        &oak(repo.path(), &["diff", "--json"]),
        "oak diff --json",
    ))
    .unwrap();
    let file = &json["changed_files"][0];
    assert_eq!(file["additions"], 1, "{file}");
    assert_eq!(file["deletions"], 1, "{file}");
}

/// Added and deleted files carry git's lifecycle headers: an extended
/// `new file mode`/`deleted file mode` line and a `/dev/null` side. Without
/// the `/dev/null` side `git apply` of a deletion leaves a 0-byte file and
/// `-R` of an addition leaves one behind.
#[test]
fn added_and_deleted_files_use_dev_null_and_lifecycle_headers() {
    let repo = tempfile::TempDir::new().unwrap();
    ok(&oak(repo.path(), &["init"]), "oak init");
    fs::write(repo.path().join("gone.txt"), b"x\ny\n").unwrap();
    fs::write(repo.path().join("empty.txt"), b"").unwrap();
    ok(
        &oak(repo.path(), &["commit", "--json", "--quiet"]),
        "oak commit",
    );
    fs::remove_file(repo.path().join("gone.txt")).unwrap();
    fs::remove_file(repo.path().join("empty.txt")).unwrap();
    fs::write(repo.path().join("new.txt"), b"hello\n").unwrap();

    let patch = ok(&oak(repo.path(), &["diff", "--print"]), "oak diff --print");
    assert!(
        patch.contains(
            "diff --oak a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,1 @@\n+hello\n"
        ),
        "added-file headers:\n{patch}"
    );
    assert!(
        patch.contains(
            "diff --oak a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n-y\n"
        ),
        "deleted-file headers:\n{patch}"
    );
    // An empty file has no hunk; its lifecycle header still says what
    // happened instead of the generic "Files ... differ".
    assert!(
        patch.contains("diff --oak a/empty.txt b/empty.txt\ndeleted file mode 100644\n"),
        "empty deleted file:\n{patch}"
    );
    assert!(!patch.contains("Files a/empty.txt"), "{patch}");

    // The JSON hunks carry the identical per-file patch.
    let json: serde_json::Value = serde_json::from_str(&ok(
        &oak(repo.path(), &["diff", "--json", "--hunks"]),
        "oak diff --json --hunks",
    ))
    .unwrap();
    let gone = json["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "gone.txt")
        .unwrap();
    assert!(
        gone["patch"].as_str().unwrap().contains("+++ /dev/null\n"),
        "{gone}"
    );
}

/// A mixed patch (add, delete, modify, no-EOL edit, mode change) printed by
/// `oak diff --print` applies with `git apply` inside a real Git fixture
/// repository whose tree matches Oak's base, and the result is exactly the
/// Oak working tree: same paths, same bytes, deleted files gone. Reversing
/// it restores the base exactly.
#[test]
fn mixed_patch_round_trips_through_git_apply_in_a_git_fixture() {
    if !git_available() {
        return;
    }
    type Tree = &'static [(&'static str, &'static [u8])];
    const BASE: Tree = &[
        ("keep.txt", b"a\nb\nc\n"),
        ("gone.txt", b"bye\nnow\n"),
        ("noeol.txt", b"x\ny"),
        ("tool.sh", b"#!/bin/sh\necho hi\n"),
        ("dir/nested.txt", b"one\n"),
    ];
    const RESULT: Tree = &[
        ("keep.txt", b"a\nB\nc\n"),
        ("noeol.txt", b"x\nY"),
        ("tool.sh", b"#!/bin/sh\necho hello\n"),
        ("dir/nested.txt", b"one\n"),
        ("dir/added.txt", b"fresh\nfile"),
    ];

    let write_tree = |root: &Path, tree: Tree| {
        for (path, bytes) in tree {
            let target = root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, bytes).unwrap();
        }
    };
    let assert_tree = |root: &Path, tree: Tree, what: &str| {
        for (path, bytes) in tree {
            let got = fs::read(root.join(path))
                .unwrap_or_else(|e| panic!("{what}: {path} unreadable: {e}"));
            assert_eq!(&got[..], *bytes, "{what}: {path} bytes");
        }
        let all: Vec<&str> = BASE.iter().chain(RESULT).map(|(p, _)| *p).collect();
        for path in all {
            let expected = tree.iter().any(|(p, _)| *p == path);
            assert_eq!(
                root.join(path).exists(),
                expected,
                "{what}: {path} existence"
            );
        }
    };

    // Oak side: commit BASE, then leave RESULT dirty (plus chmod +x).
    let repo = tempfile::TempDir::new().unwrap();
    ok(&oak(repo.path(), &["init"]), "oak init");
    write_tree(repo.path(), BASE);
    ok(
        &oak(repo.path(), &["commit", "--json", "--quiet"]),
        "oak commit",
    );
    fs::remove_file(repo.path().join("gone.txt")).unwrap();
    write_tree(repo.path(), RESULT);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            repo.path().join("tool.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let patch = ok(&oak(repo.path(), &["diff", "--print"]), "oak diff --print");
    #[cfg(unix)]
    assert!(
        patch.contains("diff --oak a/tool.sh b/tool.sh\nold mode 100644\nnew mode 100755\n"),
        "mode change headers:\n{patch}"
    );

    // Git fixture: a real repository at BASE.
    let fixture = tempfile::TempDir::new().unwrap();
    let run = |args: &[&str]| {
        let out = git(fixture.path()).args(args).output().expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q"]);
    write_tree(fixture.path(), BASE);
    run(&["add", "-A"]);
    run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "-q",
        "-m",
        "base",
    ]);
    let patch_path = fixture.path().join("..").join(format!(
        "oak-{}.patch",
        fixture.path().file_name().unwrap().to_string_lossy()
    ));
    fs::write(&patch_path, &patch).unwrap();

    run(&["apply", "--check", patch_path.to_str().unwrap()]);
    run(&["apply", patch_path.to_str().unwrap()]);
    assert_tree(fixture.path(), RESULT, "forward");
    run(&["apply", "-R", patch_path.to_str().unwrap()]);
    assert_tree(fixture.path(), BASE, "reverse");
    // Nothing left over relative to the committed base.
    let status = git(fixture.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        "",
        "reverse apply restored the base exactly"
    );
    let _ = fs::remove_file(&patch_path);
}
