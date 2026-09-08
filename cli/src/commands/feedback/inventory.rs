//! Bounded admin inventory, separate from the legacy PII-bearing export.
use super::{failed, parse_item_ref, report, resolve_remote, usage, ItemRef, LinkResult, LinksApi};
use crate::output;
use oak_core::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(super) const STATUSES: &[&str] = &[
    "nonterminal",
    "all",
    "new",
    "triaged",
    "reviewed",
    "planned",
    "in_progress",
    "blocked",
    "done",
    "wontfix",
    "duplicate",
    "spam",
];
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

// Unknown fields are intentionally ignored on input and cannot be serialized
// back out. Authored report text may itself contain private information.
#[derive(Deserialize, Serialize)]
pub(super) struct Item {
    pub(super) id: String,
    pub(super) number: i64,
    title: Option<String>,
    body: String,
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) revision: Option<String>,
    source: String,
    surface: Option<String>,
    cli_version: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize, Serialize)]
struct Scope {
    kind: String,
}

#[derive(Deserialize, Serialize)]
struct Page {
    schema_version: u32,
    scope: Scope,
    consistency: String,
    redaction: String,
    items: Vec<Item>,
    next_cursor: Option<String>,
    #[serde(skip_deserializing)]
    recommended_next_commands: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Detail {
    schema_version: u32,
    scope: Scope,
    consistency: String,
    redaction: String,
    item: Item,
}

fn valid_envelope(version: u32, scope: &Scope, consistency: &str, redaction: &str) -> bool {
    version == 1
        && scope.kind == "platform_admin"
        && consistency == "live_keyset"
        && redaction == "reporter_metadata_omitted"
}

fn valid_cursor(cursor: &str) -> bool {
    !cursor.is_empty()
        && cursor.len() <= 512
        && cursor.len().is_multiple_of(2)
        && cursor.bytes().all(|b| b.is_ascii_hexdigit())
}

fn invalid_response() -> super::FeedbackLinkError {
    failed("invalid feedback inventory response; no response data was displayed")
}

pub(super) fn valid_revision(revision: &str) -> bool {
    !revision.is_empty() && revision.len() <= 128 && !revision.chars().any(char::is_control)
}

pub(super) fn valid_item(item: &Item) -> bool {
    item.number > 0
        && !item.id.is_empty()
        && item.id.len() <= 256
        && !item.id.chars().any(char::is_control)
        && STATUSES[2..].contains(&item.status.as_str())
        && item.revision.as_deref().is_none_or(valid_revision)
}

async fn read<T: serde::de::DeserializeOwned>(
    api: &LinksApi,
    request: reqwest::RequestBuilder,
) -> LinkResult<T> {
    let mut response = api
        .send(request.timeout(std::time::Duration::from_secs(30)))
        .await?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Err(failed("feedback inventory is unsupported or unauthorized (or the item is missing); verify the server version and platform-admin login. The legacy export is not used as a fallback."));
    }
    if !status.is_success() {
        // Never display a server error body: it may echo credentials or PII.
        return Err(failed(format!(
            "feedback inventory request failed: HTTP {}",
            status.as_u16()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failed("feedback inventory response interrupted"))?
    {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(failed(
                "feedback inventory response exceeds 4 MiB; no response data was displayed",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid_response())
}

pub async fn list(
    work: &Path,
    status: String,
    limit: u16,
    cursor: Option<String>,
    remote: Option<String>,
    json: bool,
) -> Result<()> {
    report(try_list(work, status, limit, cursor, remote, json).await)
}

async fn try_list(
    work: &Path,
    status: String,
    limit: u16,
    cursor: Option<String>,
    remote: Option<String>,
    json: bool,
) -> LinkResult<()> {
    if !STATUSES.contains(&status.as_str()) || !(1..=100).contains(&limit) {
        return Err(usage("invalid feedback status or limit (expected 1..100)"));
    }
    if cursor.as_deref().is_some_and(|c| !valid_cursor(c)) {
        return Err(usage("invalid feedback cursor"));
    }
    let api = LinksApi::new(resolve_remote(work, remote.as_deref()).map_err(usage)?)?;
    let mut request = api
        .get(format!("{}/api/feedback/inventory", api.remote))
        .query(&[("status", status.as_str()), ("limit", &limit.to_string())]);
    if let Some(cursor) = &cursor {
        request = request.query(&[("cursor", cursor)]);
    }
    let mut page: Page = read(&api, request).await?;
    if !valid_envelope(
        page.schema_version,
        &page.scope,
        &page.consistency,
        &page.redaction,
    ) || page.items.len() > usize::from(limit)
        || page.items.iter().any(|item| {
            !valid_item(item)
                || (status == "nonterminal"
                    && ["done", "wontfix", "duplicate", "spam"].contains(&item.status.as_str()))
                || (status != "nonterminal" && status != "all" && item.status != status)
        })
        || page
            .items
            .windows(2)
            .any(|pair| pair[0].number <= pair[1].number)
        || page
            .next_cursor
            .as_deref()
            .is_some_and(|c| !valid_cursor(c))
    {
        return Err(invalid_response());
    }
    if let Some(cursor) = &page.next_cursor {
        page.recommended_next_commands.push(super::at_remote(
            super::feedback_command(&[
                "list",
                "--status",
                &status,
                "--limit",
                &limit.to_string(),
                "--cursor",
                cursor,
                "--json",
            ]),
            &api.remote,
        ));
    }
    if json {
        output::print_json(&page).map_err(failed)?;
    } else {
        for item in &page.items {
            let summary = item
                .title
                .as_deref()
                .unwrap_or(&item.body)
                .lines()
                .next()
                .unwrap_or_default();
            let summary: String = summary
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect();
            output::print_line(&format!(
                "fb-{}  [{}]  {}",
                item.number, item.status, summary
            ));
        }
        for command in page.recommended_next_commands {
            output::print_line(&format!("Next page: {command}"));
        }
    }
    Ok(())
}

pub async fn show(work: &Path, item: String, remote: Option<String>, json: bool) -> Result<()> {
    report(try_show(work, item, remote, json).await)
}

async fn try_show(work: &Path, item: String, remote: Option<String>, json: bool) -> LinkResult<()> {
    parse_item_ref(&item)?;
    let api = LinksApi::new(resolve_remote(work, remote.as_deref()).map_err(usage)?)?;
    let detail = read_detail(&api, &item).await?;
    if json {
        output::print_json(&detail).map_err(failed)?;
    } else {
        output::print_line(&format!(
            "fb-{}  [{}]",
            detail.item.number, detail.item.status
        ));
        if let Some(revision) = &detail.item.revision {
            output::print_line(&format!("Revision: {revision}"));
        }
        for text in [
            detail.item.title.as_deref().unwrap_or_default(),
            &detail.item.body,
        ] {
            let safe: String = text
                .chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect();
            output::print_line(&safe);
        }
    }
    Ok(())
}

pub(super) async fn resolve_item(api: &LinksApi, item: &str) -> LinkResult<Item> {
    Ok(read_detail(api, item).await?.item)
}

async fn read_detail(api: &LinksApi, item: &str) -> LinkResult<Detail> {
    let reference = parse_item_ref(item)?;
    let path = match &reference {
        ItemRef::Number(n) if *n > 0 && *n <= i64::MAX as u64 => format!("fb-{n}"),
        ItemRef::Id(id) if id.len() <= 256 => id.clone(),
        _ => return Err(usage("invalid feedback reference")),
    };
    let detail: Detail = read(api, api.get(format!("{}/api/feedback/{path}", api.remote))).await?;
    let matches = match reference {
        ItemRef::Number(n) => detail.item.number > 0 && detail.item.number as u64 == n,
        ItemRef::Id(id) => detail.item.id == id,
    };
    if !matches
        || !valid_item(&detail.item)
        || !valid_envelope(
            detail.schema_version,
            &detail.scope,
            &detail.consistency,
            &detail.redaction,
        )
    {
        return Err(invalid_response());
    }
    Ok(detail)
}
