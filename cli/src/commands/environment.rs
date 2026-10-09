//! `oak environment` (fb-523): the environment variables this binary reads,
//! their current effective values, and background network behaviour.
//!
//! The registry below is the single source for this command and is checked
//! against the source tree by a test, so a new `OAK_*` read cannot ship
//! undocumented. Secrets are reported as presence only; URL values are shown
//! only after credential-stripping normalization.
use oak_core::Result;
use serde::Serialize;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ValuePolicy {
    /// The raw value is shown.
    Shown,
    /// Only whether it is set is reported; the value is a secret.
    PresenceOnly,
    /// Shown after URL redaction (userinfo, query and fragment removed).
    NormalizedUrl,
    /// A comma-separated list of URLs, each redacted like `NormalizedUrl`.
    NormalizedUrlList,
}

pub(crate) struct EnvVar {
    pub name: &'static str,
    /// `oak` when Oak's own code reads it; `dependency` when a library Oak
    /// uses honors it (e.g. reqwest's system proxy support).
    pub read_by: &'static str,
    pub category: &'static str,
    pub policy: ValuePolicy,
    pub description: &'static str,
}

/// Environment variables Oak itself reads at runtime (test-harness-only
/// ones excluded; see `TEST_ONLY`), plus the known ones its dependencies
/// honor (`read_by: "dependency"`). A dependency may read further
/// variables this list does not know about.
pub(crate) const REGISTRY: &[EnvVar] = &[
    EnvVar {
        read_by: "oak",
        name: "OAK_REMOTE",
        category: "remote",
        policy: ValuePolicy::NormalizedUrl,
        description: "Override the stored remote URL for this invocation (same as -r). A checkout's stored key is only sent to the origin that issued it.",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_API_KEY",
        category: "credential",
        policy: ValuePolicy::PresenceOnly,
        description: "Explicit credential; wins over every stored credential and is sent to whatever server the command targets.",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_REPO",
        category: "remote",
        policy: ValuePolicy::Shown,
        description: "ORG/REPO for `oak push --repo` (first-push linking without a TTY).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_SERVE_TOKEN",
        category: "credential",
        policy: ValuePolicy::PresenceOnly,
        description: "Bearer token for `oak serve` (same as --token).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_AUTHOR",
        category: "identity",
        policy: ValuePolicy::Shown,
        description: "Commit author override (default: logged-in username, then the OS user).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_EMAIL",
        category: "identity",
        policy: ValuePolicy::Shown,
        description: "Optional contact email for `oak feedback` (after --email).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_DIFF_TOOL",
        category: "tooling",
        policy: ValuePolicy::Shown,
        description: "External diff tool over two materialized trees; must block until done, e.g. \"code --wait --diff\".",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_NO_UPDATE_CHECK",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "Any value disables the once-per-24h update check (see background_network).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_RELEASE_REPO",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "GitHub OWNER/REPO used by `oak upgrade` and the update check (default oakdotspace/oak).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_URL",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "macOS mount: base URL the FSKit mounter app is downloaded from (default https://oak.space).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_PROBE_TIMEOUT_SECS",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "Per-request bound (1..=300 s, default 20) for read-only diagnostics such as `oak auth status` and `oak repo list`.",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_UPLOAD_CONCURRENCY",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "Chunk-upload concurrency for push (positive integer).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_DOWNLOAD_CONCURRENCY",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "Chunk-download concurrency for clone/pull (positive integer, default 32).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_TRUSTED_REMOTES",
        category: "network",
        policy: ValuePolicy::NormalizedUrlList,
        description: "Comma-separated extra origins a moved remote may be auto-updated to (intended for tests).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_ALLOW_PARTIAL_CLONE",
        category: "recovery",
        policy: ValuePolicy::Shown,
        description: "Recovery only: skip blobs a broken server failed to ship instead of erroring.",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_MOUNTS_ROOT",
        category: "mount",
        policy: ValuePolicy::Shown,
        description: "Mount state directory (default ~/.oak/mounts).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_FEATURES",
        category: "features",
        policy: ValuePolicy::Shown,
        description: "Unlock feature-gated CLI subcommands (comma-separated slugs, or 1/all).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_VERBOSE",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "Print per-phase timing (same as --verbose).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_LOG",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "Enable tracing logs with this filter (e.g. debug, oak=trace).",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_PROGRESS",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "Progress display: always/on/1, never/off/0, else auto (TTY and not CI). Never animates under --json.",
    },
    EnvVar {
        read_by: "oak",
        name: "OAK_SPINNER",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "Spinner style: line, pulse, arc, minimal, oak.",
    },
    EnvVar {
        read_by: "oak",
        name: "NO_COLOR",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "Any non-empty value disables color (wins over CLICOLOR_FORCE).",
    },
    EnvVar {
        read_by: "oak",
        name: "CLICOLOR_FORCE",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "Non-empty and not \"0\" forces color even when piped.",
    },
    EnvVar {
        read_by: "oak",
        name: "CI",
        category: "output",
        policy: ValuePolicy::Shown,
        description: "When set, automatic progress display is disabled.",
    },
    EnvVar {
        read_by: "oak",
        name: "PAGER",
        category: "tooling",
        policy: ValuePolicy::Shown,
        description: "Pager for interactive diff output (default less).",
    },
    EnvVar {
        read_by: "oak",
        name: "VISUAL",
        category: "tooling",
        policy: ValuePolicy::Shown,
        description: "Editor for `oak feedback` and hook editing (before EDITOR).",
    },
    EnvVar {
        read_by: "oak",
        name: "EDITOR",
        category: "tooling",
        policy: ValuePolicy::Shown,
        description: "Editor fallback after VISUAL (then vi).",
    },
    EnvVar {
        read_by: "oak",
        name: "USER",
        category: "identity",
        policy: ValuePolicy::Shown,
        description: "OS user used as a last-resort author name.",
    },
    EnvVar {
        read_by: "oak",
        name: "USERNAME",
        category: "identity",
        policy: ValuePolicy::Shown,
        description: "OS user (Windows) used as a last-resort author name.",
    },
    EnvVar {
        read_by: "dependency",
        name: "HTTPS_PROXY",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "Proxy for HTTPS requests (reqwest system proxy; lowercase https_proxy also honored).",
    },
    EnvVar {
        read_by: "dependency",
        name: "https_proxy",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "Proxy for HTTPS requests (reqwest system proxy).",
    },
    EnvVar {
        read_by: "dependency",
        name: "HTTP_PROXY",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "Proxy for plain-HTTP requests (reqwest system proxy).",
    },
    EnvVar {
        read_by: "dependency",
        name: "http_proxy",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "Proxy for plain-HTTP requests (reqwest system proxy).",
    },
    EnvVar {
        read_by: "dependency",
        name: "ALL_PROXY",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "Proxy for all requests when no scheme-specific proxy is set (reqwest).",
    },
    EnvVar {
        read_by: "dependency",
        name: "all_proxy",
        category: "network",
        policy: ValuePolicy::NormalizedUrl,
        description: "Proxy for all requests when no scheme-specific proxy is set (reqwest).",
    },
    EnvVar {
        read_by: "dependency",
        name: "NO_PROXY",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "Hosts that bypass the proxy (reqwest).",
    },
    EnvVar {
        read_by: "dependency",
        name: "no_proxy",
        category: "network",
        policy: ValuePolicy::Shown,
        description: "Hosts that bypass the proxy (reqwest).",
    },
    EnvVar {
        read_by: "dependency",
        name: "HOME",
        category: "paths",
        policy: ValuePolicy::Shown,
        description: "Locates ~/.oak (credentials, update-check state, mounts).",
    },
    EnvVar {
        read_by: "dependency",
        name: "TMPDIR",
        category: "paths",
        policy: ValuePolicy::Shown,
        description: "Temporary directory for materialized diff trees and other temp files.",
    },
];

/// Variables Oak sets for child processes rather than reads.
pub(crate) const EXPORTED: &[(&str, &str)] =
    &[("OAK_HOOK", "Set for hook scripts to the event being run.")];

/// Read only by Oak's own test suite; never consulted by user commands.
#[cfg(test)]
pub(crate) const TEST_ONLY: &[&str] = &[
    "OAK_MERGE_BASE_PROPERTY_SEEDS",
    "OAK_WDLOCK_CONTENTION_CHILD",
    "OAK_WDLOCK_CONTENTION_DIR",
    "OAK_WDLOCK_CONTENTION_READY_DIR",
    "OAK_WDLOCK_CONTENTION_START",
    "OAK_WDLOCK_CONTENTION_STOP",
    "OAK_WDLOCK_CONTENTION_WINNERS",
    "OAK_PUBLICATION_STATE_RESERVE_CHILD",
    "OAK_PUBLICATION_STATE_RESERVE_ROOT",
    // Multi-process credentials-lock tests (commands::credentials::tests).
    "OAK_CREDENTIALS_LOCK_CHILD",
    "OAK_CREDENTIALS_LOCK_INDEX",
    "OAK_CREDENTIALS_LOCK_PATH",
    "OAK_CREDENTIALS_LOCK_START",
    // Round counts for the lock-reaping stress tests.
    "OAK_TEST_CREDSTALE_ROUNDS",
    "OAK_TEST_REAP_ROUNDS",
];

#[derive(Serialize)]
struct VariableReport {
    name: &'static str,
    read_by: &'static str,
    category: &'static str,
    set: bool,
    value_policy: ValuePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    /// Set but not representable (not UTF-8, or not a parseable URL under
    /// `normalized_url`); the raw value is never echoed.
    #[serde(skip_serializing_if = "Option::is_none")]
    value_omitted_reason: Option<&'static str>,
    description: &'static str,
}

#[derive(Serialize)]
struct UpdateCheck {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    disabled_by: Option<&'static str>,
    endpoint: String,
    interval_secs: u64,
    timeout_secs: u64,
    /// Runs synchronously after a command succeeds (not in parallel with it).
    blocking_after_success: bool,
    skipped_for: Vec<&'static str>,
    state_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_check_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_check_due_unix: Option<u64>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    kind: &'static str,
    oak_version: &'static str,
    /// What `variables` covers: every variable Oak's own code reads, plus
    /// the dependency-honored ones Oak knows about. Not exhaustive for
    /// dependencies.
    coverage: &'static str,
    variables: Vec<VariableReport>,
    exported_to_child_processes: Vec<ExportedReport>,
    background_network: BackgroundNetwork,
    recommended_next_commands: Vec<&'static str>,
}

#[derive(Serialize)]
struct ExportedReport {
    name: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct BackgroundNetwork {
    update_check: UpdateCheck,
    /// Every other network request is made only by commands whose function
    /// requires the remote (clone, fetch, pull, push, merge, ci, ...).
    other: &'static str,
}

fn report_variable(var: &EnvVar) -> VariableReport {
    let raw = std::env::var_os(var.name);
    let mut report = VariableReport {
        name: var.name,
        read_by: var.read_by,
        category: var.category,
        set: raw.is_some(),
        value_policy: var.policy,
        value: None,
        value_omitted_reason: None,
        description: var.description,
    };
    let Some(raw) = raw else {
        return report;
    };
    match var.policy {
        ValuePolicy::PresenceOnly => {}
        ValuePolicy::Shown => match raw.into_string() {
            Ok(value) => report.value = Some(value),
            Err(_) => report.value_omitted_reason = Some("not_utf8"),
        },
        ValuePolicy::NormalizedUrl => {
            match raw.into_string().ok().and_then(|value| redact_url(&value)) {
                Some(value) => report.value = Some(value),
                None => report.value_omitted_reason = Some("not_a_normalizable_url"),
            }
        }
        ValuePolicy::NormalizedUrlList => {
            let redacted = raw.into_string().ok().and_then(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(redact_url)
                    .collect::<Option<Vec<_>>>()
            });
            match redacted {
                Some(values) => report.value = Some(values.join(",")),
                None => report.value_omitted_reason = Some("not_a_normalizable_url"),
            }
        }
    }
    report
}

/// Render a URL without credentials: userinfo, query and fragment are
/// dropped. A scheme-less `host:port` (common for proxies) is read as
/// `http://`. Anything unparsable is not shown at all.
pub(crate) fn redact_url(value: &str) -> Option<String> {
    let value = value.trim();
    let mut url = reqwest::Url::parse(value)
        .ok()
        .filter(|url| url.host_str().is_some())
        .or_else(|| reqwest::Url::parse(&format!("http://{value}")).ok())?;
    url.host_str()?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.as_str().trim_end_matches('/').to_string())
}

fn update_check() -> UpdateCheck {
    let disabled = std::env::var_os("OAK_NO_UPDATE_CHECK").is_some();
    let state_file = dirs::home_dir().map(|home| home.join(".oak").join("version_check"));
    let last = state_file
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("last_check").and_then(|v| v.as_u64()));
    let interval = super::version_check::CHECK_INTERVAL_SECS;
    UpdateCheck {
        enabled: !disabled,
        disabled_by: disabled.then_some("OAK_NO_UPDATE_CHECK"),
        endpoint: format!(
            "https://github.com/{}/releases/latest",
            super::upgrade::release_repo()
        ),
        interval_secs: interval,
        timeout_secs: super::version_check::TIMEOUT_SECS,
        blocking_after_success: true,
        skipped_for: vec![
            "failed commands",
            "--json and other structured output",
            "upgrade",
            "completions",
            "file, refs, tree, environment, change",
            "branch train",
            "mount worktree-create/worktree-remove hooks",
        ],
        state_file: state_file.map(|path| path.display().to_string()),
        last_check_unix: last,
        next_check_due_unix: last.map(|last| last.saturating_add(interval)),
    }
}

pub fn run(json: bool) -> Result<()> {
    let report = Report {
        schema_version: 1,
        kind: "environment",
        oak_version: env!("CARGO_PKG_VERSION"),
        coverage: "oak_reads_plus_known_dependency_vars",
        variables: REGISTRY.iter().map(report_variable).collect(),
        exported_to_child_processes: EXPORTED
            .iter()
            .map(|(name, description)| ExportedReport { name, description })
            .collect(),
        background_network: BackgroundNetwork {
            update_check: update_check(),
            other: "none",
        },
        recommended_next_commands: vec!["oak auth status --json"],
    };
    if json {
        return crate::output::print_json(&report);
    }
    for var in &report.variables {
        let value = match (&var.value, var.set, var.value_policy) {
            (Some(value), _, _) => value.clone(),
            (None, true, ValuePolicy::PresenceOnly) => "(set; value hidden)".into(),
            (None, true, _) => "(set; not shown)".into(),
            (None, false, _) => "(unset)".into(),
        };
        crate::output::print_line(&format!("{:<26} {value}", var.name));
        crate::output::print_line(&format!("{:<26} {}", "", var.description));
    }
    let check = &report.background_network.update_check;
    crate::output::print_line("");
    crate::output::print_line(&format!(
        "Update check: {} — at most once per {} h after a successful command, {} s timeout, {}",
        if check.enabled {
            "enabled"
        } else {
            "disabled (OAK_NO_UPDATE_CHECK)"
        },
        check.interval_secs / 3600,
        check.timeout_secs,
        check.endpoint
    ));
    crate::output::print_line("No other background network activity.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_never_echoes_userinfo() {
        for (input, expected) in [
            (
                "http://user:pw@proxy.test:3128",
                Some("http://proxy.test:3128"),
            ),
            (
                "socks5://u:SEKRET@h:1080/?t=SEKRET#SEKRET",
                Some("socks5://h:1080"),
            ),
            ("u:SEKRET@proxy.test:8080", Some("http://proxy.test:8080")),
            ("https://oak.space/", Some("https://oak.space")),
        ] {
            assert_eq!(redact_url(input).as_deref(), expected, "{input}");
        }
        assert!(redact_url("::not a url::").is_none_or(|v| !v.contains("SEKRET")));
    }

    /// Every `OAK_*` name the CLI and core sources mention must be either in
    /// the registry, exported to children, or explicitly test-only.
    #[test]
    fn registry_covers_every_oak_env_var_in_source() {
        let mut found = std::collections::BTreeSet::new();
        let roots = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../core/src"),
        ];
        let mut stack: Vec<std::path::PathBuf> = roots.iter().map(Into::into).collect();
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (index, _) in text.match_indices("\"OAK_") {
                        let rest = &text[index + 1..];
                        let end = rest
                            .find(|c: char| {
                                !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                            })
                            .unwrap_or(rest.len());
                        if end > "OAK_".len() && rest[end..].starts_with('"') {
                            found.insert(rest[..end].to_string());
                        }
                    }
                }
            }
        }
        let known: std::collections::BTreeSet<_> = REGISTRY
            .iter()
            .map(|var| var.name)
            .chain(EXPORTED.iter().map(|(name, _)| *name))
            .chain(TEST_ONLY.iter().copied())
            .map(str::to_string)
            .collect();
        let missing: Vec<_> = found.difference(&known).collect();
        assert!(missing.is_empty(), "undocumented env vars: {missing:?}");
    }
}
