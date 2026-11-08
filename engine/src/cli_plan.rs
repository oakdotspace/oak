use serde_json::{json, Value};

use crate::args::Args;
use crate::error::EngineError;

/// Local operations: status, log, diff and the like.
const LOCAL_MS: u64 = 30_000;
/// Anything that talks to the server. Commit is local, but it hashes content,
/// and game projects carry large binaries, so it gets this budget too.
const LONG_MS: u64 = 300_000;

/// Every action `cli.plan` and `cli.parse` accept.
pub const ACTIONS: &[&str] = &[
    "status",
    "info",
    "diff",
    "log",
    "branch_list",
    "branch_create",
    "branch_switch",
    "branch_close",
    "commit",
    "desc_get",
    "desc_append",
    "fetch",
    "pull",
    "push",
    "merge",
    "restore",
    "whoami",
    "ci_status",
    "ci_runs",
    "ci_logs",
    "ci_publish_preview",
    "ci_publish",
    "conflict_status",
    "version",
];

struct Plan {
    argv: Vec<String>,
    stdin: Option<String>,
    timeout_ms: u64,
}

impl Plan {
    fn new(argv: &[&str], timeout_ms: u64) -> Self {
        Plan {
            argv: argv.iter().map(|arg| arg.to_string()).collect(),
            stdin: None,
            timeout_ms,
        }
    }

    fn arg(mut self, arg: &str) -> Self {
        self.argv.push(arg.to_string());
        self
    }

    fn flag(self, on: bool, flag: &str) -> Self {
        if on {
            self.arg(flag)
        } else {
            self
        }
    }

    fn option(self, flag: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.arg(flag).arg(value),
            None => self,
        }
    }

    /// Positional paths, after `--` so a name can't be read as a flag.
    fn paths(self, paths: &[String]) -> Self {
        if paths.is_empty() {
            return self;
        }
        paths
            .iter()
            .fold(self.arg("--"), |plan, path| plan.arg(path))
    }
}

/// A repo-relative path with forward slashes. Oak resolves these against the
/// repo root, so an absolute or escaping path is a caller bug.
fn repo_paths(params: &Args) -> Result<Vec<String>, EngineError> {
    params
        .str_list("paths")?
        .into_iter()
        .map(|path| {
            let path = path.replace('\\', "/");
            let escapes = path.split('/').any(|part| part == "..");
            let windows_absolute = path.as_bytes().get(1) == Some(&b':');
            if path.is_empty() || path.starts_with('/') || escapes || windows_absolute {
                return Err(EngineError::bad_request(format!(
                    "`{path}` is not a repo-relative path"
                )));
            }
            if path.contains('\0') {
                return Err(EngineError::bad_request("a path holds a NUL byte"));
            }
            Ok(path)
        })
        .collect()
}

fn required_paths(params: &Args) -> Result<Vec<String>, EngineError> {
    let paths = repo_paths(params)?;
    if paths.is_empty() {
        return Err(EngineError::bad_request("`paths` must not be empty"));
    }
    Ok(paths)
}

/// A value that goes in a positional or flag slot. It can't start with `-`.
fn name<'a>(params: &Args<'a>, key: &str) -> Result<&'a str, EngineError> {
    let value = params.str(key)?;
    checked_name(key, value)
}

fn opt_name<'a>(params: &Args<'a>, key: &str) -> Result<Option<&'a str>, EngineError> {
    params
        .opt_str(key)?
        .map(|value| checked_name(key, value))
        .transpose()
}

fn checked_name<'a>(key: &str, value: &'a str) -> Result<&'a str, EngineError> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_whitespace) {
        return Err(EngineError::bad_request(format!(
            "`{key}` is not a valid name: `{value}`"
        )));
    }
    Ok(value)
}

fn full_hash<'a>(value: &'a str, key: &str) -> Result<&'a str, EngineError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(value)
    } else {
        Err(EngineError::bad_request(format!(
            "`{key}` must be a full 64-character hash"
        )))
    }
}

fn publish_base(params: &Args) -> Result<Plan, EngineError> {
    let plan = Plan::new(&["ci", "publish"], LONG_MS)
        .option("-e", Some(name(params, "environment")?))
        .option("--workflow", opt_name(params, "workflow")?)
        .option("--branch", opt_name(params, "branch")?);
    Ok(plan)
}

fn build(action: &str, params: &Args) -> Result<Plan, EngineError> {
    Ok(match action {
        "status" => Plan::new(&["status", "--json"], LOCAL_MS),
        "info" | "desc_get" => Plan::new(&["info", "--json"], LOCAL_MS),
        "version" => Plan::new(&["--version"], LOCAL_MS),
        "conflict_status" => Plan::new(&["conflict", "status", "--json"], LOCAL_MS),
        "diff" => {
            let max_bytes = params.opt_u64("max_bytes")?.map(|n| n.to_string());
            let plan = Plan::new(&["diff", "--json", "--hunks"], LOCAL_MS)
                .option("--max-bytes", max_bytes.as_deref());
            // Both or neither: `oak diff HASH` alone compares the commit to the
            // working tree, which would pass for a view of that commit.
            let plan = match (opt_name(params, "from")?, opt_name(params, "to")?) {
                (Some(from), Some(to)) => plan.arg(from).arg(to),
                (None, None) => plan,
                _ => {
                    return Err(EngineError::bad_request(
                        "`from` and `to` go together: oak can't diff a commit with no parent",
                    ))
                }
            };
            plan.paths(&repo_paths(params)?)
        }
        "log" => {
            let limit = params.opt_u64("limit")?.unwrap_or(50).to_string();
            Plan::new(&["log", "--json", "-n", &limit], LOCAL_MS).paths(&repo_paths(params)?)
        }
        "branch_list" => Plan::new(&["branch", "list", "--json"], LOCAL_MS)
            .option("--status", opt_name(params, "status")?),
        "branch_create" => {
            // Keeps dirty files unless `clean`. No name means a generated one.
            let plan = Plan::new(&["switch", "-c"], LONG_MS).flag(params.bool("clean")?, "--clean");
            match opt_name(params, "name")? {
                Some(branch) => plan.arg(branch),
                None => plan,
            }
        }
        "branch_switch" => Plan::new(&["switch"], LONG_MS).arg(name(params, "name")?),
        "branch_close" => Plan::new(&["close", "--json"], LONG_MS).arg(name(params, "name")?),
        "commit" => {
            Plan::new(&["commit", "--json", "--quiet"], LONG_MS).paths(&repo_paths(params)?)
        }
        "desc_append" => {
            let message = params.str("message")?;
            if message.trim().is_empty() {
                return Err(EngineError::bad_request("`message` is empty"));
            }
            // Over stdin, so no temp file and no quoting of user text.
            let mut plan = Plan::new(&["desc", "--append", "--file", "-", "--json"], LONG_MS);
            plan.stdin = Some(message.to_string());
            plan
        }
        "fetch" => Plan::new(&["fetch"], LONG_MS),
        "pull" => {
            let (resume, abort) = (params.bool("continue")?, params.bool("abort")?);
            if resume && abort {
                return Err(EngineError::bad_request("`continue` and `abort` conflict"));
            }
            Plan::new(&["pull"], LONG_MS)
                .flag(resume, "--continue")
                .flag(abort, "--abort")
                .flag(!resume && !abort, "--json")
        }
        "push" => Plan::new(&["push", "--json"], LONG_MS),
        "merge" => {
            let wait = params.opt_u64("wait_minutes")?;
            let timeout = wait.map_or(LONG_MS, |minutes| {
                minutes.saturating_mul(60_000).saturating_add(LONG_MS)
            });
            let plan = Plan::new(&["merge", "--json"], timeout);
            match wait {
                // `--wait` takes an optional value, so it must be attached.
                Some(minutes) => plan.arg(&format!("--wait={minutes}")),
                None => plan,
            }
        }
        "restore" => {
            // Noninteractive discard, so never without paths: an empty list
            // would restore the whole tree.
            Plan::new(&["restore", "-f"], LONG_MS)
                .option("-s", opt_name(params, "source")?)
                .paths(&required_paths(params)?)
        }
        // `oak whoami` has no JSON and fails on a stored-credential miss with
        // text only; `auth status` reports the same as data.
        "whoami" => Plan::new(&["auth", "status", "--json"], LONG_MS),
        "ci_status" => {
            let run = params.opt_u64("run")?.map(|n| n.to_string());
            let commit = params.opt_str("commit")?;
            if let Some(commit) = commit {
                full_hash(commit, "commit")?;
            }
            Plan::new(&["ci", "status", "--json"], LONG_MS)
                .option("--run", run.as_deref())
                .option("--commit", commit)
        }
        "ci_runs" => {
            let limit = params.opt_u64("limit")?.unwrap_or(20).to_string();
            Plan::new(&["ci", "runs", "--json", "--limit", &limit], LONG_MS)
        }
        "ci_logs" => {
            let run = params.u64("run")?.to_string();
            let max_bytes = params.opt_u64("max_bytes")?.map(|n| n.to_string());
            Plan::new(&["ci", "logs", &run, "--json"], LONG_MS)
                .flag(params.bool("summary")?, "--summary")
                .flag(params.bool("failed")?, "--failed")
                .option("--max-bytes", max_bytes.as_deref())
        }
        "ci_publish_preview" => publish_base(params)?.arg("--dry-run").arg("--json"),
        "ci_publish" => {
            // `commit` is the preview's, required either way so a caller can't
            // publish without having one. Only a republish names it: `--commit`
            // marks the authorization as a republish, even at the branch head.
            let commit = full_hash(params.str("commit")?, "commit")?;
            let commit = params.bool("republish")?.then_some(commit);
            publish_base(params)?
                .option("--commit", commit)
                .arg("--yes")
                .arg("--json")
        }
        other => {
            return Err(EngineError::bad_request(format!(
                "unknown action `{other}`"
            )))
        }
    })
}

/// `cli.plan`: what to run. `argv` excludes the binary (`binary.candidates`
/// finds that) and always runs from the repo root.
pub fn plan(args: &Args) -> Result<Value, EngineError> {
    let action = args.str("action")?;
    let params = args.object("params")?;
    let plan = build(action, &params)?;
    Ok(json!({
        "argv": plan.argv,
        "cwd_relative": ".",
        "stdin": plan.stdin,
        "timeout_ms": plan.timeout_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(action: &str, params: Value) -> Result<Value, EngineError> {
        let args = json!({"action": action, "params": params});
        plan(&Args::new(&args).unwrap())
    }

    fn argv(action: &str, params: Value) -> String {
        run(action, params).unwrap()["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn refused(action: &str, params: Value) {
        assert!(run(action, params.clone()).is_err(), "{action} {params}");
    }

    #[test]
    fn every_action_plans_with_minimal_params() {
        let hash = "a".repeat(64);
        for action in ACTIONS {
            let params = json!({
                "name": "x", "paths": ["a"], "message": "m", "run": 1,
                "environment": "production", "commit": hash,
            });
            let plan = run(action, params).unwrap();
            assert_eq!(plan["cwd_relative"], ".");
            assert!(plan["timeout_ms"].as_u64().unwrap() >= LOCAL_MS);
            assert!(!plan["argv"].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn read_only_commands() {
        assert_eq!(argv("status", json!({})), "status --json");
        assert_eq!(argv("desc_get", json!({})), "info --json");
        assert_eq!(argv("whoami", json!({})), "auth status --json");
        assert_eq!(
            argv("diff", json!({"max_bytes": 100, "paths": ["a b.txt"]})),
            "diff --json --hunks --max-bytes 100 -- a b.txt"
        );
        assert_eq!(argv("log", json!({})), "log --json -n 50");
        assert_eq!(
            argv("log", json!({"limit": 5, "paths": ["a"]})),
            "log --json -n 5 -- a"
        );
        assert_eq!(
            argv("branch_list", json!({"status": "open"})),
            "branch list --json --status open"
        );
    }

    #[test]
    fn diff_between_two_revisions() {
        assert_eq!(
            argv("diff", json!({"from": "abc", "to": "def", "paths": ["a"]})),
            "diff --json --hunks abc def -- a"
        );
        refused("diff", json!({"to": "def"}));
        refused("diff", json!({"from": "abc"}));
        refused("diff", json!({"from": "--stat", "to": "def"}));
    }

    #[test]
    fn branch_commands() {
        assert_eq!(argv("branch_create", json!({})), "switch -c");
        assert_eq!(
            argv("branch_create", json!({"name": "x", "clean": true})),
            "switch -c --clean x"
        );
        assert_eq!(argv("branch_switch", json!({"name": "x"})), "switch x");
        assert_eq!(argv("branch_close", json!({"name": "x"})), "close --json x");
        refused("branch_switch", json!({"name": "--force"}));
        refused("branch_switch", json!({"name": "a b"}));
        refused("branch_switch", json!({"name": ""}));
        refused("branch_switch", json!({}));
    }

    #[test]
    fn user_text_goes_over_stdin() {
        let plan = run("desc_append", json!({"message": "--help\nmore"})).unwrap();
        assert_eq!(
            plan["argv"],
            json!(["desc", "--append", "--file", "-", "--json"])
        );
        assert_eq!(plan["stdin"], "--help\nmore");
        refused("desc_append", json!({"message": " "}));
        assert_eq!(run("status", json!({})).unwrap()["stdin"], Value::Null);
    }

    #[test]
    fn paths_are_checked_and_never_flags() {
        assert_eq!(
            argv("commit", json!({"paths": ["--force", "a\\b.txt"]})),
            "commit --json --quiet -- --force a/b.txt"
        );
        assert_eq!(argv("commit", json!({})), "commit --json --quiet");
        for bad in ["/etc/passwd", "../x", "a/../../b", "C:\\x", "", "a\0b"] {
            refused("commit", json!({"paths": [bad]}));
        }
    }

    #[test]
    fn restore_refuses_the_whole_tree() {
        refused("restore", json!({}));
        assert_eq!(argv("restore", json!({"paths": ["a"]})), "restore -f -- a");
        assert_eq!(
            argv("restore", json!({"paths": ["a"], "source": "HEAD"})),
            "restore -f -s HEAD -- a"
        );
    }

    #[test]
    fn sync_commands() {
        assert_eq!(argv("pull", json!({})), "pull --json");
        assert_eq!(argv("pull", json!({"continue": true})), "pull --continue");
        assert_eq!(argv("pull", json!({"abort": true})), "pull --abort");
        refused("pull", json!({"continue": true, "abort": true}));
        assert_eq!(argv("merge", json!({})), "merge --json");
        let waiting = run("merge", json!({"wait_minutes": 10})).unwrap();
        assert_eq!(waiting["argv"], json!(["merge", "--json", "--wait=10"]));
        assert_eq!(waiting["timeout_ms"], 900_000);
    }

    #[test]
    fn only_a_republish_names_its_commit() {
        let hash = "b".repeat(64);
        assert_eq!(
            argv(
                "ci_publish_preview",
                json!({"environment": "production", "workflow": "release"})
            ),
            "ci publish -e production --workflow release --dry-run --json"
        );
        assert_eq!(
            argv(
                "ci_publish",
                json!({"environment": "production", "branch": "main", "commit": hash})
            ),
            "ci publish -e production --branch main --yes --json"
        );
        assert_eq!(
            argv(
                "ci_publish",
                json!({"environment": "production", "commit": hash, "republish": true})
            ),
            format!("ci publish -e production --commit {hash} --yes --json")
        );
        refused("ci_publish", json!({"environment": "production"}));
        refused(
            "ci_publish",
            json!({"environment": "production", "commit": "abc"}),
        );
        refused("ci_publish_preview", json!({}));
    }

    #[test]
    fn ci_reads() {
        let hash = "c".repeat(64);
        assert_eq!(argv("ci_status", json!({})), "ci status --json");
        assert_eq!(
            argv("ci_status", json!({"run": 3, "commit": hash})),
            format!("ci status --json --run 3 --commit {hash}")
        );
        refused("ci_status", json!({"commit": "x"}));
        assert_eq!(argv("ci_runs", json!({})), "ci runs --json --limit 20");
        assert_eq!(
            argv("ci_logs", json!({"run": 3, "failed": true, "max_bytes": 9})),
            "ci logs 3 --json --failed --max-bytes 9"
        );
        refused("ci_logs", json!({}));
    }

    #[test]
    fn unknown_action_and_bad_params() {
        refused("rebase", json!({}));
        let args = json!({"action": "status", "params": 1});
        assert!(plan(&Args::new(&args).unwrap()).is_err());
    }
}
