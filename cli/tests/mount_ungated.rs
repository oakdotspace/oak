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
