//! Integration tests for Oak space scaffolding and agent-facing contracts.

use std::path::Path;
use std::process::{Command, Output};

fn oak(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("oak binary should run")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn space_repos_without_org_or_marker_is_usage_error() {
    let temp = tempfile::TempDir::new().unwrap();

    let out = oak(
        temp.path(),
        &["space", "repos", "--remote", "http://127.0.0.1:9"],
    );

    assert_eq!(
        out.status.code(),
        Some(2),
        "expected usage exit code\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("no org given and no .oak-space marker found here"),
        "stderr should explain the missing space marker:\n{}",
        stderr(&out)
    );
}

#[test]
fn space_new_scaffolds_org_space_with_finish_references() {
    let temp = tempfile::TempDir::new().unwrap();
    let space = temp.path().join("acme-space");

    let out = oak(
        temp.path(),
        &[
            "space",
            "new",
            "acme/blog",
            space.to_str().unwrap(),
            "--remote",
            "http://127.0.0.1:9",
        ],
    );

    assert!(
        out.status.success(),
        "space new should scaffold despite repo-list warning\nstdout:\n{}\nstderr:\n{}",
        stdout(&out),
        stderr(&out)
    );

    assert_eq!(
        std::fs::read_to_string(space.join(".oak-space")).unwrap(),
        "acme\n"
    );
    assert!(space.join("CLAUDE.md").exists());
    assert!(space.join(".claude/settings.json").exists());

    let agents = std::fs::read_to_string(space.join("AGENTS.md")).unwrap();
    assert!(
        agents.contains("oak finish --desc-file \"$DESC_FILE\""),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak mount finish [path] --desc-file <file>"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak desc --file <file>"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak mount list --json"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak agent state --json"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak agent state --json --compact"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak status --porcelain"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak status --short"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak diff --name-only"),
        "AGENTS.md:\n{agents}"
    );
    assert!(
        agents.contains("oak log --oneline -n N"),
        "AGENTS.md:\n{agents}"
    );

    let settings = std::fs::read_to_string(space.join(".claude/settings.json")).unwrap();
    assert!(settings.contains("Bash(oak finish:*)"));
}
