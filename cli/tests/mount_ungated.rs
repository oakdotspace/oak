//! `oak mount` is not behind a feature flag.
//!
//! It used to be gated behind `OAK_FEATURES=mount`, which — among other things
//! — wedged the FSKit broker that launchd spawns without that env, hanging
//! `/sbin/mount`. The gate has been removed; this pins that mount stays usable
//! with no `OAK_FEATURES` set so the gate can't quietly come back.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::process::Command;

const OAK: &str = env!("CARGO_BIN_EXE_oak");

#[test]
fn mount_command_runs_without_oak_features() {
    let out = Command::new(OAK)
        .args(["mount", "list"])
        .env_remove("OAK_FEATURES")
        .output()
        .expect("run oak mount list");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains("feature is gated"),
        "`oak mount` must not be behind a feature flag: {text}"
    );
    assert!(
        out.status.success(),
        "`oak mount list` should succeed without OAK_FEATURES: {text}"
    );
}

/// Bare `oak mount` (no target) must NOT mass-mount every repo on the site.
/// Instead it prints the subcommand help and exits 0, without contacting the
/// remote — proven here by pointing at an unreachable server and still
/// succeeding. The old `--all-visible` bulk-mount escape hatch is gone.
#[test]
fn bare_mount_prints_help_without_mounting() {
    let out = Command::new(OAK)
        .args(["mount", "--remote", "http://127.0.0.1:9"])
        .env_remove("OAK_FEATURES")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run bare oak mount");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        out.status.success(),
        "bare `oak mount` should succeed by printing help: {text}"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "bare `oak mount` should exit 0 (printed help, did not mount): {text}"
    );
    assert!(
        text.contains("Usage:") && text.contains("Omit to print this help"),
        "bare `oak mount` should print the mount help text: {text}"
    );
    assert!(
        !text.contains("all-visible"),
        "the `--all-visible` bulk-mount flag should no longer exist: {text}"
    );
}

/// A bare `oak mount <repo>` (no `/`) is a single repo in your org, resolved
/// from your login exactly like `oak clone <repo>` — NOT a bulk mount of the
/// whole org. With no login for the target remote, resolution fails asking you
/// to log in or name the org, proving it never bulk-mounts. (Unreachable remote
/// so the test can't accidentally contact a real server.)
#[test]
fn mount_bare_repo_resolves_like_clone() {
    let out = Command::new(OAK)
        .args(["mount", "somerepo", "--remote", "http://127.0.0.1:9"])
        .env_remove("OAK_FEATURES")
        .env_remove("OAK_AUTHOR")
        .env("OAK_NO_UPDATE_CHECK", "1")
        .output()
        .expect("run oak mount <repo>");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !out.status.success(),
        "`oak mount <repo>` with no login should fail, not bulk-mount: {text}"
    );
    assert!(
        text.contains("oak login") && text.contains("<org>/somerepo"),
        "error should mirror clone: log in or name the org explicitly: {text}"
    );
}
