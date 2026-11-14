//! Source scan: tests must never pop UI on the developer's Mac.
//!
//! Running a binary out of a fake `Foo.app` bundle makes macOS show the
//! logged-in user a modal "Foo.app is damaged and can't be opened" alert, once
//! per test run. `open`, `osascript` and friends reach the real desktop the
//! same way. This is a mechanical check over the Rust sources, so it is a
//! heuristic: it matches text, not behavior. Keep the allowlists small and
//! give every entry a reason.

use std::path::{Path, PathBuf};

/// Things that launch apps, browsers or dialogs. Production code may use them
/// only at the sites listed in [`LAUNCHER_ALLOWLIST`]; test code never.
const LAUNCHERS: &[&str] = &[
    "/usr/bin/open",
    "osascript",
    "Command::new(\"open\")",
    "open::that",
];

/// `(file, needle, max occurrences, reason)` for production code.
const LAUNCHER_ALLOWLIST: &[(&str, &str, usize, &str)] = &[
    (
        "cli/src/commands/mount/fskit/mod.rs",
        "/usr/bin/open",
        1,
        "open_status_app: only an installed Oak Mount app, skipped under OAK_NO_MOUNT_TRAY",
    ),
    (
        "cli/src/commands/mount/fskit/mod.rs",
        "Command::new(\"open\")",
        1,
        "ensure_mounter_ready: registers the app right after a first install",
    ),
    (
        "cli/src/commands/login.rs",
        "open::that",
        1,
        "browser handoff for `login`; tests set OAK_BROWSER",
    ),
    (
        "cli/src/commands/open.rs",
        "open::that",
        1,
        "the `open` command; tests pass --print or --json",
    ),
];

/// Never allowed in test code: a bundle's executable dir, a path inside a
/// bundle, the real /Applications, and tools that change the user's session.
const TEST_FORBIDDEN: &[&str] = &[
    "Contents/MacOS",
    ".app/",
    "\"/Applications",
    "Command::new(\"launchctl\")",
    "Command::new(\"pluginkit\")",
    // A copied Apple-signed binary is killed at launch (platform signature
    // check); symlink it instead.
    "copy(\"/bin/",
    "copy(\"/usr/bin/",
];

/// Naming an app bundle at all in test code is limited to these files.
const BUNDLE_MENTIONS: &[&str] = &["APP_BUNDLE", ".app\""];
const BUNDLE_MENTION_ALLOWLIST: &[(&str, &str)] = &[(
    "cli/src/commands/mount/fskit/mod.rs",
    "install/replace tests build and ditto-zip bundle dirs holding one marker file; nothing in them runs",
)];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn tests_do_not_launch_apps_or_run_inside_app_bundles() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for root in ["cli/src", "cli/tests", "core/src", "core/tests"] {
        rust_files(&workspace.join(root), &mut files);
    }
    assert!(files.len() > 50, "scan found only {} files", files.len());

    let mut violations = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(workspace)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "cli/src/no_ui_in_tests.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&file).unwrap();
        // Test code: a whole file under `tests/`, else everything from the
        // first `#[cfg(test)]` on (test modules sit at the end of a file).
        let mut in_test = rel.contains("/tests/");
        let mut launcher_counts = vec![0usize; LAUNCHERS.len()];
        for (index, line) in source.lines().enumerate() {
            let code = line.trim();
            in_test |= code.starts_with("#[cfg(test)]");
            if code.starts_with("//") {
                continue;
            }
            let at = format!("{rel}:{}", index + 1);
            for (launcher, count) in LAUNCHERS.iter().zip(&mut launcher_counts) {
                if !code.contains(launcher) {
                    continue;
                }
                if in_test {
                    violations.push(format!("{at}: test code uses `{launcher}`"));
                } else {
                    *count += 1;
                }
            }
            if !in_test {
                continue;
            }
            for needle in TEST_FORBIDDEN {
                if code.contains(needle) {
                    violations.push(format!("{at}: test code uses `{needle}`"));
                }
            }
            let allowed = BUNDLE_MENTION_ALLOWLIST.iter().any(|(f, _)| *f == rel);
            for needle in BUNDLE_MENTIONS {
                if code.contains(needle) && !allowed {
                    violations.push(format!(
                        "{at}: test code names an app bundle (`{needle}`) outside the allowlist"
                    ));
                }
            }
        }
        for (launcher, count) in LAUNCHERS.iter().zip(launcher_counts) {
            let allowed = LAUNCHER_ALLOWLIST
                .iter()
                .find(|(f, n, _, _)| *f == rel && n == launcher)
                .map_or(0, |(_, _, max, _)| *max);
            if count > allowed {
                violations.push(format!(
                    "{rel}: {count} use(s) of `{launcher}`, allowlist permits {allowed}"
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "tests must not pop UI or run anything inside a `.app` bundle \
         (see cli/src/no_ui_in_tests.rs):\n{}",
        violations.join("\n")
    );
}
