//! `oak report`: free text about a commit, filed under a name (`tests`,
//! `server-prod`, `review`). Its first line is the headline History shows;
//! the rest is on the commit's page. The next report under the same name
//! replaces it. Whether a commit passed something is a mark's move, not a
//! report.
//!
//! Reports live on the server (`/api/:owner/:name/commits/:commit/reports`).
//! `oak mark report` is a hidden alias for filing one, and `oak mark follow`
//! files its "deploying", "live" and "failed" reports through the same code.

use std::path::Path;
use std::time::Duration;

use oak_core::{OakError, Result};
use serde_json::{json, Value};

use super::access::RepoTarget;
use super::mark::send;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// What a report filing sends.
#[derive(Debug, Default, Clone)]
pub struct ReportArgs {
    pub commit: String,
    pub name: String,
    /// Free text; its first line is the headline.
    pub body: String,
    pub url: Option<String>,
    pub move_id: Option<i64>,
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn reports_url(target: &RepoTarget, commit: &str) -> String {
    target.url(&format!("commits/{}/reports", urlencoding::encode(commit)))
}

/// The report's text from `--body` or `--body-file` (`-` reads stdin).
pub fn read_body(body: Option<String>, body_file: Option<&Path>) -> Result<String> {
    Ok(match (body, body_file) {
        (Some(b), _) => b,
        (None, Some(p)) if p.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        }
        (None, Some(p)) => String::from_utf8_lossy(&std::fs::read(p)?).into_owned(),
        (None, None) => String::new(),
    })
}

/// File a report; the server's answer (the report as stored).
pub async fn post_report(target: &RepoTarget, args: &ReportArgs) -> Result<Value> {
    let mut body = json!({ "name": args.name, "body": args.body });
    if let Some(u) = &args.url {
        body["url"] = json!(u);
    }
    if let Some(m) = args.move_id {
        body["move_id"] = json!(m);
    }
    send(
        target,
        target
            .request(
                reqwest::Method::POST,
                reports_url(target, &args.commit),
                REQUEST_TIMEOUT,
            )
            .json(&body),
    )
    .await
}

/// `oak report COMMIT --name N --body TEXT` (and `oak mark report`).
pub async fn file(target: RepoTarget, args: ReportArgs, json: bool) -> Result<()> {
    if args.name.trim().is_empty() {
        return Err(OakError::InvalidArgument(
            "a report needs a name (--name)".to_string(),
        ));
    }
    let value = post_report(&target, &args).await?;
    if json {
        let commit = value["commit"].as_str().unwrap_or(&args.commit).to_string();
        let doc = json!({
            "schema_version": SCHEMA_VERSION,
            "repo": target.slug(),
            "report": value,
            "recommended_next_commands": vec![format!(
                "oak report {} --repo {} --json",
                output::shell_quote(&commit),
                output::shell_quote(&target.slug())
            )],
        });
        return output::print_json(&doc);
    }
    output::success(&format!(
        "Reported {} on {}: {}",
        args.name,
        short(value["commit"].as_str().unwrap_or(&args.commit)),
        value["headline"].as_str().unwrap_or("")
    ));
    Ok(())
}

/// `oak report COMMIT [--name N]`: the commit's reports, or the one under
/// that name with its whole body.
pub async fn list(target: RepoTarget, commit: &str, name: Option<&str>, json: bool) -> Result<()> {
    let value = send(
        &target,
        target.request(
            reqwest::Method::GET,
            reports_url(&target, commit),
            REQUEST_TIMEOUT,
        ),
    )
    .await?;
    let resolved = value["commit"].as_str().unwrap_or(commit).to_string();
    let mut reports: Vec<Value> = value["reports"].as_array().cloned().unwrap_or_default();
    if let Some(n) = name {
        reports.retain(|r| r["name"].as_str() == Some(n));
    }
    if json {
        return output::print_json(&json!({
            "schema_version": SCHEMA_VERSION,
            "repo": target.slug(),
            "commit": resolved,
            "reports": reports,
            "recommended_next_commands": Vec::<String>::new(),
        }));
    }
    if reports.is_empty() {
        match name {
            Some(n) => output::print_line(&format!("No {n} report on {}.", short(&resolved))),
            None => output::print_line(&format!("No reports on {}.", short(&resolved))),
        }
        return Ok(());
    }
    if name.is_some() {
        for r in &reports {
            output::print_line(&format!(
                "{} on {}, by {} at {}",
                r["name"].as_str().unwrap_or(""),
                short(&resolved),
                r["reporter"].as_str().unwrap_or(""),
                r["updated_at"].as_str().unwrap_or("")
            ));
            if let Some(u) = r["url"].as_str() {
                output::print_line(u);
            }
            output::print_line("");
            output::print_line(r["body"].as_str().unwrap_or("").trim_end());
        }
        return Ok(());
    }
    for r in &reports {
        output::print_line(&format!(
            "{:<20} {}  ({}, {})",
            r["name"].as_str().unwrap_or(""),
            r["headline"].as_str().unwrap_or(""),
            r["reporter"].as_str().unwrap_or(""),
            r["updated_at"].as_str().unwrap_or("")
        ));
    }
    Ok(())
}
