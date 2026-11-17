fn help_for(args: &[&str]) -> String {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("COLUMNS", "80")
        .output()
        .expect("run oak help");
    assert!(output.status.success(), "{args:?} failed");
    String::from_utf8(output.stdout).expect("utf-8 help")
}

/// Help text with its wrapping undone (every run of whitespace becomes one
/// space), so a phrase can be found wherever the terminal width broke it.
fn unwrapped(help: &str) -> String {
    help.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `oak --help` lists command names and one-line descriptions, every row
/// within 80 columns; each command's flags live in its own `--help`.
#[test]
fn handwritten_top_level_help_fits_in_80_columns() {
    let help = help_for(&[]);
    for line in help.lines() {
        assert!(
            line.chars().count() <= 80,
            "line over 80 columns: {line:?}\n{help}"
        );
    }
    for command in [
        "clone",
        "doctor",
        "retention",
        "change",
        "mark",
        "root",
        "desc",
        "finish",
    ] {
        assert!(
            help.lines().any(|l| l.trim_start().starts_with(command)),
            "missing {command:?} in:\n{help}"
        );
    }
}

/// The integrity and scoped-clone flags the overview used to spell out are
/// all in the per-command help.
#[test]
fn integrity_workflow_flags_are_in_command_help() {
    for (args, expected) in [
        (
            &["clone", "--help"][..],
            &[
                "--branch <NAME>",
                "--detached",
                "--expected-head <FULL>",
                "--path <PREFIX>",
                "--shallow",
                "--allow-unverified-integrity",
                "--allow-legacy-scope",
            ][..],
        ),
        (&["doctor", "--help"], &["--repo <ORG/REPO>", "--verify"]),
        (
            &["doctor", "blob", "--help"],
            &["--repo <ORG/REPO>", "--depth"],
        ),
        (&["change", "export", "--help"], &["--output <FILE>"]),
        (&["doctor", "file", "--help"], &["--at <", "--json"]),
        (&["root", "pull", "--help"], &["--dir", "--force"]),
    ] {
        let help = help_for(args);
        for flag in expected {
            assert!(
                help.contains(flag),
                "{args:?} help is missing {flag:?}:\n{help}"
            );
        }
    }
    let clone = help_for(&["clone", "--help"]);
    assert!(
        !clone.contains("--allow-unverified-integrity|--allow-legacy-scope"),
        "independent clone policy flags must not render as mutually exclusive:\n{clone}"
    );
}

#[test]
fn clone_help_describes_selected_branch_and_narrowed_scope() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["clone", "--help"])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run clone help");
    assert!(output.status.success());
    let help = unwrapped(&String::from_utf8(output.stdout).expect("utf-8 help"));
    assert!(help.contains("selected branch"), "{help}");
    assert!(help.contains("--expected-head <FULL>"), "{help}");
    assert!(help.contains("--json"), "{help}");
    assert!(help.contains("acquisition receipt"), "{help}");
    assert!(help.contains("exact full Oak commit hash"), "{help}");
    assert!(
        help.contains("narrows local history plus preflight/recovery scope"),
        "{help}"
    );
    assert!(
        !help.contains("purely a download-speed/disk optimization"),
        "{help}"
    );
}

#[test]
fn agent_review_contract_flags_are_discoverable() {
    let push = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["push", "--help"])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run push help");
    assert!(push.status.success());
    let push = String::from_utf8(push.stdout).unwrap();
    assert!(push.contains("--json"), "{push}");
}

fn oak_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run oak")
}

#[test]
fn merge_help_discloses_server_side_squash_merge() {
    // fb-92: `oak merge` mutates the remote; the help must say so.
    let temp = tempfile::TempDir::new().unwrap();
    let out = oak_in(temp.path(), &["merge", "--help"]);
    assert!(out.status.success());
    let help = unwrapped(&String::from_utf8(out.stdout).expect("utf-8 help"));
    assert!(
        help.contains("Squash-merge a branch into main on the server"),
        "{help}"
    );
    assert!(help.contains("advancing the remote main"), "{help}");
    assert!(help.contains("not a local-only merge"), "{help}");
    assert!(!help.contains("stack"), "{help}");
    assert!(help.contains("closing the"), "{help}");
    // Nothing gates a merge on the server; marks record what passed.
    assert!(help.contains("oak mark"), "{help}");
    assert!(!help.contains("CI-gated"), "{help}");
    assert!(!help.contains("oak ci"), "{help}");
    assert!(!help.contains("--wait"), "{help}");
}

#[test]
fn merge_wait_followed_by_branch_name_is_a_usage_error_with_the_fix() {
    // fb-363: `--wait` takes an optional value, so a branch after it is
    // parsed as the timeout. Fail before any mutation and name the fix.
    let temp = tempfile::TempDir::new().unwrap();
    let out = oak_in(temp.path(), &["merge", "--wait", "mrmrs-abc123"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("oak merge mrmrs-abc123 --wait"), "{err}");
    assert!(err.contains("--wait=N"), "{err}");

    // Numeric forms still parse: both reach the post-parse dry-run guard.
    for args in [
        ["merge", "--wait", "15", "--dry-run", "--json"].as_slice(),
        ["merge", "--wait=15", "--dry-run", "--json"].as_slice(),
        ["merge", "feature", "--wait", "--dry-run", "--json"].as_slice(),
    ] {
        let out = oak_in(temp.path(), args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("cannot be combined with --dry-run"),
            "{args:?} should parse --wait: {text}"
        );
    }
}

#[test]
fn oak_help_shows_the_same_overview_as_bare_oak() {
    let run = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(args)
            .env("OAK_NO_UPDATE_CHECK", "1")
            .output()
            .expect("run oak");
        assert!(output.status.success(), "{args:?} failed");
        String::from_utf8(output.stdout).expect("utf-8 help")
    };
    let bare = run(&[]);
    assert!(bare.contains("Getting started"), "{bare}");
    assert_eq!(run(&["help"]), bare);
    // `oak help <command>` still renders clap's per-command help.
    assert!(run(&["help", "commit"]).contains("Usage: oak commit"));
}

#[test]
fn ci_and_workflow_say_they_were_removed() {
    let temp = tempfile::TempDir::new().unwrap();
    for args in [
        &["ci", "status"][..],
        &["ci", "publish", "deploy", "--yes", "--wait"],
        &["workflow", "list"],
        &["--verbose", "ci"],
    ] {
        let out = oak_in(temp.path(), args);
        assert_ne!(out.status.code(), Some(0), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(err.trim().lines().count(), 1, "{args:?}: {err}");
        assert!(err.contains("removed with Oak CI"), "{err}");
        assert!(err.contains("oak mark"), "{err}");
    }
    let out = oak_in(temp.path(), &["ci", "status", "--json"]);
    assert_ne!(out.status.code(), Some(0));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert!(doc["error"]["message"]
        .as_str()
        .unwrap()
        .contains("removed with Oak CI"));
    // Not in the overview either.
    let help = help_for(&[]);
    assert!(
        !help.lines().any(|l| l.trim_start().starts_with("ci ")),
        "{help}"
    );
    assert!(
        !help
            .lines()
            .any(|l| l.trim_start().starts_with("workflow ")),
        "{help}"
    );
}

/// Commands that were removed or moved under `oak doctor` exit 2 with one
/// line naming the replacement, and appear in no help or completion script.
#[test]
fn removed_and_moved_commands_name_their_replacement() {
    let temp = tempfile::TempDir::new().unwrap();
    for (args, replacement) in [
        (&["checkout", "abcd1234"][..], "oak switch --detach"),
        (&["rev-parse", "--short", "HEAD"], "oak hash"),
        (&["info"], "oak status --json"),
        (&["archive", "-o", "x.zip"], "oak export --tree-only"),
        (&["refs", "inspect"], "oak doctor refs"),
        (&["tree", "inspect", "--at", "HEAD"], "oak doctor tree"),
        (&["file", "inspect", "--at", "HEAD", "a"], "oak doctor file"),
        (&["blob", "info", "abc", "--repo", "a/b"], "oak doctor blob"),
        (&["maintenance", "compact"], "oak doctor compact"),
        (&["branch", "close", "feature"], "oak close"),
        (&["--verbose", "info"], "oak status --json"),
    ] {
        let out = oak_in(temp.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(err.trim().lines().count(), 1, "{args:?}: {err}");
        assert!(err.contains(replacement), "{args:?}: {err}");
    }
    let out = oak_in(temp.path(), &["refs", "inspect", "--json"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert!(doc["error"]["message"]
        .as_str()
        .unwrap()
        .contains("oak doctor refs"));

    let help = help_for(&[]);
    let completions = help_for(&["completions", "bash"]);
    for removed in [
        "checkout",
        "rev-parse",
        "info",
        "archive",
        "refs",
        "tree",
        "file",
        "blob",
        "maintenance",
    ] {
        assert!(
            !help
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{removed} "))),
            "{removed} in help:\n{help}"
        );
        assert!(
            !completions.contains(&format!("oak,{removed})")),
            "{removed} in completions"
        );
    }
    assert!(!completions.contains("oak__subcmd__branch,close)"));
    for moved in ["refs", "tree", "file", "blob", "compact"] {
        assert!(
            completions.contains(&format!("oak__subcmd__doctor,{moved})")),
            "doctor {moved} missing from completions"
        );
    }
}

/// `oak doctor` with flags is the remote integrity check it always was; the
/// subcommands are local diagnostics.
#[test]
fn doctor_keeps_its_flags_and_gains_subcommands() {
    let help = help_for(&["doctor", "--help"]);
    for expected in [
        "--repo <ORG/REPO>",
        "--verify",
        "refs",
        "tree",
        "file",
        "blob",
        "compact",
    ] {
        assert!(help.contains(expected), "{expected} missing:\n{help}");
    }
    let temp = tempfile::TempDir::new().unwrap();
    let out = oak_in(temp.path(), &["doctor"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--repo"));
}
