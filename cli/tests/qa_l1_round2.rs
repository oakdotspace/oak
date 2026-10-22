//! QA-L1 round 2 acceptance test, adopted from independent QA of
//! mrmrs-0a1cc5. RED at fe7347d9: an inline description starting with `-`
//! yielded a literal retry command that clap parsed as flags (for
//! `--file=PATH`, publishing PATH's contents). Must stay GREEN.

use std::path::Path;
use std::process::{Command, Output};

use oak_core::{MetadataKey, Repository, SqliteRepository};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn oak_env(dir: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oak"))
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("OAK_NO_UPDATE_CHECK", "1")
        .env("OAK_AUTHOR", "tester")
        .env_remove("OAK_REMOTE")
        .env_remove("OAK_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// Split a POSIX-quoted command line as `sh` would (single quotes and
/// `'\''` splices only, which is all `shell_quote` emits).
fn sh_words(command: &str) -> Vec<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "for a in {command}; do printf '%s\\0' \"$a\"; done"
        ))
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .split('\0')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn qa_inline_retry_command_round_trips_dash_leading_text() {
    let reject = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&reject)
        .await;
    for text in [
        "--help",
        "--file=/etc/hosts",
        "-m note",
        "--looks-like-flag",
    ] {
        let repo_dir = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        oak_cli::commands::init::run(repo_dir.path(), false).unwrap();
        let repo = SqliteRepository::open(&repo_dir.path().join(".oak/oak.db")).unwrap();
        repo.set_metadata(MetadataKey::RemoteUrl, &reject.uri())
            .unwrap();
        repo.set_metadata(MetadataKey::RepoOwner, "qa").unwrap();
        repo.set_metadata(MetadataKey::RepoName, "widget").unwrap();
        let (dir, home_path) = (repo_dir.path().to_path_buf(), home.path().to_path_buf());
        let owned = text.to_string();
        let output = tokio::task::spawn_blocking(move || {
            oak_env(&dir, &home_path, &["desc", "--json", "--", &owned])
        })
        .await
        .unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"));
        assert_eq!(receipt["remote_synced"], false);
        let Some(retry) = receipt["retry_command"].as_str() else {
            continue; // no literal command offered: acceptable
        };
        // Replaying the command must parse to the same description: its
        // argv must carry the text as a positional (after `--`), not a flag.
        let words = sh_words(retry);
        let separator = words.iter().position(|w| w == "--");
        let text_index = words.iter().rposition(|w| w == text);
        assert!(
            matches!((separator, text_index), (Some(s), Some(t)) if s < t),
            "retry command for {text:?} does not pass it positionally: {retry}"
        );
    }
}
