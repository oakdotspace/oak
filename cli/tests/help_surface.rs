#[test]
fn handwritten_top_level_help_exposes_integrity_workflows() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run oak help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("utf-8 help");
    for expected in [
        "clone       [ORG/REPO] [--branch NAME",
        "--branch NAME [--expected-head FULL]",
        "--allow-unverified-integrity] [--allow-legacy-scope",
        "doctor      --repo ORG/REPO",
        "--verify metadata|existence|bytes",
        "blob info   HASH --repo ORG/REPO",
        "[--depth N [--branch NAME]]",
        "change      capture",
        "export CAPTURE_ID --output FILE",
        "cancel RUN_ID --commit HASH",
        "inventory [ROOT]",
        "file        inspect --at HEAD|HASH --json PATH",
    ] {
        assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
    }
    assert!(
        !help.contains("--allow-unverified-integrity|--allow-legacy-scope"),
        "independent clone policy flags must not render as mutually exclusive:\n{help}"
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
    let help = String::from_utf8(output.stdout).expect("utf-8 help");
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
    let ci = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(["ci", "wait", "--help"])
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run ci wait help");
    assert!(ci.status.success());
    let ci = String::from_utf8(ci.stdout).unwrap();
    assert!(ci.contains("--commit <HASH>"), "{ci}");
    assert!(ci.contains("--timeout <TIMEOUT>"), "{ci}");
    assert!(ci.contains("--json"), "{ci}");

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
    let help = String::from_utf8(out.stdout).expect("utf-8 help");
    assert!(
        help.contains("Squash-merge a branch into main on the server"),
        "{help}"
    );
    assert!(help.contains("advancing the remote main"), "{help}");
    assert!(help.contains("When the parent is main"), "{help}");
    assert!(help.contains("closing the"), "{help}");
    assert!(help.contains("oak merge BRANCH --wait"), "{help}");
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
