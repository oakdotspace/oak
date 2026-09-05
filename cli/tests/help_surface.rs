#[test]
fn handwritten_top_level_help_exposes_integrity_workflows() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_oak"))
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run oak help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("utf-8 help");
    for expected in [
        "clone       [ORG/REPO] [--shallow]",
        "--allow-unverified-integrity] [--allow-legacy-scope",
        "doctor      --repo ORG/REPO",
        "--verify metadata|existence|bytes",
        "blob info   HASH --repo ORG/REPO",
        "[--depth N [--branch NAME]]",
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
