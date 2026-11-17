//! Git import (`oak init --import-git`, `oak clone <git-url>`) and
//! `oak export`, end to end through the real binary and the real `git`.
//!
//! The strongest check is the round trip: a git history imported into Oak and
//! exported again comes back as the same git commits, ids and all.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

/// A sandbox with its own HOME, so neither git nor oak reads the developer's
/// config, credentials or hooks.
struct Sandbox {
    temp: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("home")).unwrap();
        Sandbox { temp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn oak(&self, dir: &Path, args: &[&str]) -> Output {
        self.oak_env(dir, args, &[])
    }

    fn oak_env(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oak"));
        cmd.args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env("OAK_AUTHOR", "tester")
            .env("GIT_CONFIG_GLOBAL", self.home().join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("OAK_API_KEY")
            .env_remove("OAK_REMOTE")
            .stdin(std::process::Stdio::null());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.output().expect("run oak")
    }

    fn oak_ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.oak(dir, args);
        assert!(
            out.status.success(),
            "oak {args:?} failed ({}):\nstdout: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn oak_json(&self, dir: &Path, args: &[&str]) -> Value {
        let stdout = self.oak_ok(dir, args);
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"))
    }

    /// `git` with a fixed identity and clock: every commit made through here
    /// has the same author and committer, at `date` seconds, in UTC.
    fn git_at(&self, dir: &Path, date: i64, args: &[&str]) -> String {
        let stamp = format!("{date} +0000");
        let out = Command::new("git")
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("GIT_CONFIG_GLOBAL", self.home().join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .env("GIT_AUTHOR_DATE", &stamp)
            .env("GIT_COMMITTER_DATE", &stamp)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.git_at(dir, 1_700_000_000, args)
    }
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    fs::write(full, content).unwrap();
}

#[cfg(unix)]
fn make_executable(dir: &Path, path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let full = dir.join(path);
    let mut perms = fs::metadata(&full).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(full, perms).unwrap();
}

/// A git history with most of what real repositories hold: nested files,
/// an executable, a symlink, names with spaces, quotes and non-ASCII, a
/// rename, a delete, a tracked file an ignore pattern matches, line-ending
/// attributes, and a merge of two lines of work.
#[cfg(unix)]
fn rich_git_repo(sb: &Sandbox, dir: &Path) {
    let clock = std::cell::Cell::new(1_700_000_000i64);
    let tick = || {
        clock.set(clock.get() + 60);
        clock.get()
    };
    let commit = |sb: &Sandbox, msg: &str| {
        let t = tick();
        sb.git_at(dir, t, &["add", "-A", "-f"]);
        sb.git_at(dir, t, &["commit", "-q", "-m", msg]);
    };
    fs::create_dir_all(dir).unwrap();
    sb.git(dir, &["init", "-q"]);
    write(dir, "README.md", "# demo\n");
    write(dir, "src/lib.rs", "pub fn a() {}\n");
    write(dir, "scripts/run.sh", "#!/bin/sh\necho hi\n");
    make_executable(dir, "scripts/run.sh");
    std::os::unix::fs::symlink("README.md", dir.join("link-to-readme")).unwrap();
    commit(sb, "first");

    write(dir, "docs/a file with spaces.md", "spaces\n");
    write(dir, "docs/\"quoted\".txt", "quotes\n");
    write(dir, "docs/日本語.txt", "unicode\n");
    write(dir, ".gitignore", "build/\n");
    write(dir, "build/keep.txt", "tracked though ignored\n");
    write(dir, ".gitattributes", "*.bat text eol=crlf\n");
    write(dir, "win.bat", "echo one\necho two\n");
    commit(sb, "second\n\nwith a body\n");

    sb.git(dir, &["checkout", "-q", "-b", "side"]);
    write(dir, "side.txt", "from side\n");
    commit(sb, "side work");
    sb.git(dir, &["checkout", "-q", "main"]);
    fs::rename(dir.join("src/lib.rs"), dir.join("src/core.rs")).unwrap();
    fs::remove_file(dir.join("docs/\"quoted\".txt")).unwrap();
    commit(sb, "rename and delete");
    sb.git_at(
        dir,
        tick(),
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    write(dir, "after.txt", "after merge\n");
    commit(sb, "after merge");
}

fn git_log(sb: &Sandbox, dir: &Path) -> Vec<String> {
    sb.git(dir, &["log", "--format=%H", "--topo-order", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect()
}

#[cfg(unix)]
#[test]
fn import_then_export_reproduces_the_same_git_commits() {
    let sb = Sandbox::new();
    let source = sb.path("source");
    rich_git_repo(&sb, &source);
    let original = git_log(&sb, &source);

    // Import a copy, so the source stays a reference.
    let work = sb.path("work");
    sb.git(
        sb.temp.path(),
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    let receipt = sb.oak_json(&work, &["init", "--import-git", "--json"]);
    assert_eq!(receipt["imported_commit_count"], original.len());
    assert_eq!(
        receipt["git_import"]["skipped_paths"],
        serde_json::json!([])
    );
    assert_eq!(receipt["git_import"]["shallow_git_history"], false);

    let out = sb.path("out");
    sb.oak_ok(
        &work,
        &["export", out.to_str().unwrap(), "--git-branch", "main"],
    );
    assert_eq!(git_log(&sb, &out), original, "same commits, same ids");
    assert_eq!(
        sb.git(&out, &["rev-parse", "HEAD^{tree}"]),
        sb.git(&source, &["rev-parse", "HEAD^{tree}"])
    );
    // The export's working tree is checked out and clean.
    assert_eq!(sb.git(&out, &["status", "--porcelain"]), "");
    assert_eq!(
        fs::read_to_string(out.join("build/keep.txt")).unwrap(),
        "tracked though ignored\n"
    );
    assert!(fs::symlink_metadata(out.join("link-to-readme"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn import_leaves_the_working_tree_and_uncommitted_work_alone() {
    let sb = Sandbox::new();
    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.git(&dir, &["init", "-q"]);
    write(&dir, "a.txt", "committed\n");
    sb.git(&dir, &["add", "-A"]);
    sb.git(&dir, &["commit", "-q", "-m", "one"]);
    write(&dir, "a.txt", "edited, not committed\n");
    write(&dir, "new.txt", "untracked\n");

    sb.oak_json(&dir, &["init", "--import-git", "--json"]);
    assert_eq!(
        fs::read_to_string(dir.join("a.txt")).unwrap(),
        "edited, not committed\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("new.txt")).unwrap(),
        "untracked\n"
    );
    let status = sb.oak_json(&dir, &["status", "--json"]);
    let changed: Vec<&str> = status["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert!(changed.contains(&"a.txt"), "{status}");
    assert!(changed.contains(&"new.txt"), "{status}");
}

#[test]
fn init_without_a_terminal_asks_for_a_decision_about_git_history() {
    let sb = Sandbox::new();
    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.git(&dir, &["init", "-q"]);
    write(&dir, "a.txt", "a\n");
    sb.git(&dir, &["add", "-A"]);
    sb.git(&dir, &["commit", "-q", "-m", "one"]);

    let out = sb.oak(&dir, &["init", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["error"]["code"], "invalid_argument");
    assert!(doc["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--import-git"));
    assert!(!dir.join(".oak").exists(), "nothing created");

    // Starting empty beside git is an explicit choice.
    let receipt = sb.oak_json(&dir, &["init", "--no-import-git", "--json"]);
    assert_eq!(receipt["imported_commit_count"], 0);
    assert!(dir.join(".git").exists());
}

#[test]
fn import_git_needs_a_git_repository() {
    let sb = Sandbox::new();
    let dir = sb.path("plain");
    fs::create_dir_all(&dir).unwrap();
    let out = sb.oak(&dir, &["init", "--import-git", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!dir.join(".oak").exists());
}

#[test]
fn importing_an_empty_git_repository_makes_an_empty_oak_repository() {
    let sb = Sandbox::new();
    let dir = sb.path("empty");
    fs::create_dir_all(&dir).unwrap();
    sb.git(&dir, &["init", "-q"]);
    let receipt = sb.oak_json(&dir, &["init", "--import-git", "--json"]);
    assert_eq!(receipt["imported_commit_count"], 0);
    sb.oak_json(&dir, &["status", "--json"]);
}

#[test]
fn a_shallow_git_clone_imports_from_its_oldest_fetched_commit() {
    let sb = Sandbox::new();
    let source = sb.path("source");
    fs::create_dir_all(&source).unwrap();
    sb.git(&source, &["init", "-q"]);
    for i in 0..5 {
        write(&source, "n.txt", &format!("{i}\n"));
        sb.git_at(&source, 1_700_000_000 + i, &["add", "-A"]);
        sb.git_at(
            &source,
            1_700_000_000 + i,
            &["commit", "-q", "-m", &format!("c{i}")],
        );
    }
    let shallow = sb.path("shallow");
    let url = format!("file://{}", source.display());
    sb.git(
        sb.temp.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "2",
            &url,
            shallow.to_str().unwrap(),
        ],
    );

    let receipt = sb.oak_json(&shallow, &["init", "--import-git", "--json"]);
    assert_eq!(receipt["imported_commit_count"], 2);
    assert_eq!(receipt["git_import"]["shallow_git_history"], true);

    let out = sb.path("out");
    sb.oak_ok(
        &shallow,
        &["export", out.to_str().unwrap(), "--git-branch", "main"],
    );
    // The oldest commit has no parent in the export (git's shallow commit
    // still names one), so ids differ from there on; the content doesn't.
    assert_eq!(git_log(&sb, &out).len(), 2);
    assert_eq!(
        sb.git(&out, &["rev-parse", "HEAD^{tree}"]),
        sb.git(&shallow, &["rev-parse", "HEAD^{tree}"])
    );
    assert_eq!(
        sb.git(&out, &["log", "--format=%s"]),
        sb.git(&shallow, &["log", "--format=%s"])
    );
}

/// Paths git allows but Oak can't track are left out of every commit, listed
/// in the receipt, and the rest of the history still imports and exports.
#[test]
fn untrackable_paths_are_left_out_and_reported() {
    let sb = Sandbox::new();
    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.git(&dir, &["init", "-q"]);
    write(&dir, "ok.txt", "fine\n");
    write(&dir, ".oak/hooks/pre-commit", "#!/bin/sh\nexit 1\n");
    write(&dir, ".oak/attributes", "*.bin binary\n");
    write(&dir, "nested/.oak/oak.db", "not a repository\n");
    write(&dir, "deep/.oakroot", "x\n");
    sb.git(&dir, &["add", "-A", "-f"]);
    sb.git(&dir, &["commit", "-q", "-m", "plants things"]);
    // Gone from the checkout before import, still in the history.
    sb.git(&dir, &["rm", "-q", "-r", ".oak/hooks", "nested", "deep"]);
    sb.git(&dir, &["commit", "-q", "-m", "removes them"]);
    fs::remove_dir_all(dir.join(".oak")).ok();
    sb.git(&dir, &["checkout", "-q", "--", ".oak/attributes"]);

    let receipt = sb.oak_json(&dir, &["init", "--import-git", "--json"]);
    assert_eq!(receipt["imported_commit_count"], 2);
    let skipped: Vec<&str> = receipt["git_import"]["skipped_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["path"].as_str().unwrap())
        .collect();
    assert!(skipped.contains(&".oak/hooks"), "{skipped:?}");
    assert!(skipped.contains(&"nested/.oak"), "{skipped:?}");
    assert!(skipped.contains(&"deep/.oakroot"), "{skipped:?}");
    assert!(!skipped.contains(&".oak/attributes"), "{skipped:?}");

    let out = sb.path("out");
    sb.oak_ok(&dir, &["export", out.to_str().unwrap()]);
    let first = sb.git(&out, &["ls-tree", "-r", "--name-only", "HEAD~1"]);
    assert_eq!(
        first.lines().collect::<Vec<_>>(),
        [".oak/attributes", "ok.txt"],
        "the first commit keeps what Oak can track"
    );
}

/// `oak clone <git-url>` refuses a repository whose checkout would plant a
/// hook Oak runs, and leaves nothing behind.
#[test]
fn clone_refuses_a_planted_hook_and_cleans_up() {
    let sb = Sandbox::new();
    let source = sb.path("source");
    fs::create_dir_all(&source).unwrap();
    sb.git(&source, &["init", "-q"]);
    write(&source, ".oak/hooks/pre-commit", "#!/bin/sh\n");
    write(&source, "a.txt", "a\n");
    sb.git(&source, &["add", "-A", "-f"]);
    sb.git(&source, &["commit", "-q", "-m", "hook"]);

    let dest = sb.path("dest");
    let url = format!("file://{}/.git", source.display());
    let out = sb.oak(sb.temp.path(), &["clone", &url, dest.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(".oak/hooks"));
    assert!(!dest.exists(), "the destination is removed");
}

#[test]
fn clone_imports_a_git_url() {
    let sb = Sandbox::new();
    let source = sb.path("source");
    fs::create_dir_all(&source).unwrap();
    sb.git(&source, &["init", "-q"]);
    write(&source, "a.txt", "a\n");
    sb.git(&source, &["add", "-A"]);
    sb.git(&source, &["commit", "-q", "-m", "one"]);

    let dest = sb.path("dest");
    let url = format!("file://{}/.git", source.display());
    sb.oak_ok(sb.temp.path(), &["clone", &url, dest.to_str().unwrap()]);
    assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "a\n");
    let status = sb.oak_json(&dest, &["status", "--json", "--compact"]);
    assert_eq!(status["dirty"], false, "{status}");

    let out = sb.path("out");
    sb.oak_ok(
        &dest,
        &["export", out.to_str().unwrap(), "--git-branch", "main"],
    );
    assert_eq!(git_log(&sb, &out), git_log(&sb, &source));
}

/// Oak's own history (checkpoints with no message, deletes, mode changes)
/// exports commit for commit, and exporting twice gives the same ids.
#[cfg(unix)]
#[test]
fn export_of_oak_history_is_faithful_and_deterministic() {
    let sb = Sandbox::new();
    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.oak_json(&dir, &["init", "--json"]);
    write(&dir, "a.txt", "one\n");
    write(&dir, "dir/b.txt", "b\n");
    sb.oak_ok(&dir, &["commit"]);
    make_executable(&dir, "a.txt");
    fs::remove_file(dir.join("dir/b.txt")).unwrap();
    write(&dir, "c.txt", "c\n");
    sb.oak_ok(&dir, &["commit"]);

    let one = sb.path("one");
    let two = sb.path("two");
    sb.oak_ok(&dir, &["export", one.to_str().unwrap()]);
    sb.oak_ok(&dir, &["export", two.to_str().unwrap()]);
    assert_eq!(git_log(&sb, &one).len(), 2);
    assert_eq!(git_log(&sb, &one), git_log(&sb, &two), "deterministic");

    let tip = sb.git(&one, &["ls-tree", "-r", "HEAD"]);
    assert!(
        tip.contains("100755 blob") && tip.contains("\ta.txt"),
        "{tip}"
    );
    assert!(tip.contains("\tc.txt"), "{tip}");
    assert!(!tip.contains("dir/b.txt"), "{tip}");
    let first = sb.git(&one, &["ls-tree", "-r", "--name-only", "HEAD~1"]);
    assert_eq!(first.lines().collect::<Vec<_>>(), ["a.txt", "dir/b.txt"]);
    // A checkpoint has no message; git gets a placeholder naming it.
    assert!(sb
        .git(&one, &["log", "-1", "--format=%s"])
        .starts_with("oak: "));
}

/// Export writes every tracked file, whatever the user's git setup says:
/// ignore patterns, a global hook that would fail every commit, signing.
#[test]
fn export_is_not_swayed_by_ignore_rules_hooks_or_signing() {
    let sb = Sandbox::new();
    let hooks = sb.path("hooks");
    write(&hooks, "pre-commit", "#!/bin/sh\nexit 1\n");
    #[cfg(unix)]
    make_executable(&hooks, "pre-commit");
    fs::write(
        sb.home().join(".gitconfig"),
        format!(
            "[core]\n\thooksPath = {}\n[commit]\n\tgpgsign = true\n",
            hooks.display()
        ),
    )
    .unwrap();

    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.oak_json(&dir, &["init", "--json"]);
    write(&dir, ".gitignore", "*.log\nout/\n");
    write(&dir, "debug.log", "tracked\n");
    write(&dir, ".oakignore", "");
    write(&dir, "out/artifact.txt", "tracked\n");
    sb.oak_ok(&dir, &["commit"]);

    let out = sb.path("out");
    sb.oak_ok(&dir, &["export", out.to_str().unwrap()]);
    let files = sb.git(&out, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(files.lines().any(|l| l == "debug.log"), "{files}");
    assert!(files.lines().any(|l| l == "out/artifact.txt"), "{files}");
}

#[test]
fn export_refuses_a_non_empty_destination_and_a_bad_branch_name() {
    let sb = Sandbox::new();
    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.oak_json(&dir, &["init", "--json"]);
    write(&dir, "a.txt", "a\n");
    sb.oak_ok(&dir, &["commit"]);

    let busy = sb.path("busy");
    write(&busy, "keep.txt", "mine\n");
    let out = sb.oak(&dir, &["export", busy.to_str().unwrap()]);
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(busy.join("keep.txt")).unwrap(), "mine\n");
    assert!(!busy.join(".git").exists());

    let fresh = sb.path("fresh");
    let out = sb.oak(
        &dir,
        &[
            "export",
            fresh.to_str().unwrap(),
            "--git-branch",
            "bad..name",
        ],
    );
    assert!(!out.status.success());
    assert!(!fresh.exists(), "nothing written for a bad branch name");
}

/// `--force` into an existing git repository rewrites the branch and leaves
/// that repository's working tree alone.
#[test]
fn export_force_overwrites_the_branch_in_an_existing_repository() {
    let sb = Sandbox::new();
    let dir = sb.path("repo");
    fs::create_dir_all(&dir).unwrap();
    sb.oak_json(&dir, &["init", "--json"]);
    write(&dir, "a.txt", "a\n");
    sb.oak_ok(&dir, &["commit"]);

    let target = sb.path("target");
    fs::create_dir_all(&target).unwrap();
    sb.git(&target, &["init", "-q"]);
    write(&target, "theirs.txt", "theirs\n");
    sb.git(&target, &["add", "-A"]);
    sb.git(&target, &["commit", "-q", "-m", "theirs"]);
    sb.git(&target, &["branch", "exported"]);

    sb.oak_ok(
        &dir,
        &[
            "export",
            target.to_str().unwrap(),
            "--git-branch",
            "exported",
            "--force",
        ],
    );
    assert_eq!(
        sb.git(&target, &["ls-tree", "-r", "--name-only", "exported"]),
        "a.txt"
    );
    assert_eq!(sb.git(&target, &["rev-list", "--count", "exported"]), "1");
    assert_eq!(
        fs::read_to_string(target.join("theirs.txt")).unwrap(),
        "theirs\n"
    );
}
