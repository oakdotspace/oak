//! One explicit conditional mutation. Never replace the caller's precondition
//! with a later observation, retry a mutation, or fall back to legacy PATCH.
use super::{failed, inventory, parse_item_ref, resolve_remote, usage, LinkResult, LinksApi};
use crate::output;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

const PROTOCOL: &str = "feedback_transition_v1";
const MAX_RESPONSE: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct Options {
    pub item: String,
    pub expected_revision: String,
    pub status: Option<String>,
    pub notes: Option<String>,
    pub clear_notes: bool,
    pub remote: Option<String>,
    pub json: bool,
}

#[derive(Deserialize)]
struct Capability {
    schema_version: u32,
    // The server deliberately omits this field while transitions are disabled.
    // Missing means unavailable; a present value must still be a numeric version.
    #[serde(default)]
    feedback_transition_v1: u32,
}

#[derive(Serialize)]
struct FailureContext {
    transition_sent: bool,
    outcome: &'static str,
    #[serde(skip)]
    code: oak_core::ErrorCode,
    #[serde(skip)]
    next_commands: Vec<String>,
}

#[derive(Deserialize)]
struct WireReceipt {
    schema_version: u32,
    protocol: String,
    changed: bool,
    previous_revision: String,
    revision: String,
    item: inventory::Item,
    admin_notes: serde_json::Value,
}

#[derive(Serialize)]
struct Receipt {
    schema_version: u32,
    protocol: &'static str,
    confirmed: bool,
    changed: bool,
    feedback_id: String,
    feedback_ref: String,
    status: String,
    previous_revision: String,
    revision: String,
}

fn unconfirmed(refresh: &str) -> super::FeedbackLinkError {
    failed(format!("UNCONFIRMED: feedback transition may have committed, but no valid receipt was received. Run `{refresh}`, review the current state, and reconcile before retrying. Do not automatically replay or substitute a new revision."))
}

async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> std::result::Result<T, ()> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > MAX_RESPONSE.saturating_sub(bytes.len()) {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

pub async fn run(work: &Path, options: Options) -> oak_core::Result<()> {
    let json = options.json;
    let mut context = FailureContext {
        transition_sent: false,
        outcome: "not_sent",
        code: oak_core::ErrorCode::FeedbackTransitionPreflightFailed,
        next_commands: Vec::new(),
    };
    // One error, reported by `main`'s epilogue in either mode. The contract
    // mapping decides exit (usage 2, failed 1) and renders the envelope; the
    // code is the typed one this run reached, and `feedback_transition`
    // says what was sent.
    try_run(work, options, &mut context).await.map_err(|error| {
        let usage = matches!(error, super::FeedbackLinkError::Usage(_));
        let transition = json.then(|| serde_json::to_value(&context).ok()).flatten();
        oak_core::OakError::FeedbackFailed {
            message: error.to_string(),
            usage,
            code: if usage {
                oak_core::ErrorCode::InvalidArgument
            } else {
                context.code
            },
            retryable: None,
            next_commands: context.next_commands.clone(),
            transition,
        }
    })
}

async fn try_run(work: &Path, options: Options, context: &mut FailureContext) -> LinkResult<()> {
    parse_item_ref(&options.item)?;
    if !inventory::valid_revision(&options.expected_revision) {
        return Err(usage("expected revision must be a nonempty opaque token of at most 128 bytes without control characters"));
    }
    if options
        .status
        .as_deref()
        .is_some_and(|status| !inventory::STATUSES[2..].contains(&status))
    {
        return Err(usage("invalid feedback status; use one of the ten lifecycle statuses shown by `oak feedback list`"));
    }
    if (options.notes.is_some() && options.clear_notes)
        || (options.status.is_none() && options.notes.is_none() && !options.clear_notes)
    {
        return Err(usage(
            "supply --status and/or one of --notes or --clear-notes",
        ));
    }
    if options
        .notes
        .as_deref()
        .is_some_and(|notes| notes.chars().count() > 20_000 || notes.contains('\0'))
    {
        return Err(usage(
            "notes must contain at most 20,000 Unicode characters and no NUL bytes",
        ));
    }
    let api = LinksApi::new(resolve_remote(work, options.remote.as_deref()).map_err(usage)?)?;
    let refresh = super::at_remote(
        super::feedback_command(&["show", &options.item, "--json"]),
        &api.remote,
    );
    context.next_commands.push(refresh.clone());
    context.code = oak_core::ErrorCode::FeedbackCapabilityRequestFailed;
    let capability = api
        .send(
            api.get(format!("{}/api/feedback/capabilities", api.remote))
                .timeout(TIMEOUT),
        )
        .await?;
    if capability.status() != reqwest::StatusCode::OK {
        let status = capability.status().as_u16();
        context.code = match status {
            401 | 403 => oak_core::ErrorCode::FeedbackCapabilityUnauthorized,
            404 | 405 => oak_core::ErrorCode::FeedbackCapabilityUnavailable,
            300..=399 => oak_core::ErrorCode::FeedbackCapabilityRedirect,
            _ => oak_core::ErrorCode::FeedbackCapabilityRequestFailed,
        };
        return Err(failed(format!("feedback capability discovery returned HTTP {status}; the endpoint may be unavailable or access denied. No transition was sent. Check platform-admin login and server support with the operator; redirects and legacy mutation fallback are not supported.")));
    }
    context.code = oak_core::ErrorCode::FeedbackCapabilityInvalid;
    let capability: serde_json::Value = bounded_json(capability)
        .await
        .map_err(|_| failed("invalid feedback capability response; no transition was sent"))?;
    if !capability.is_object() {
        return Err(failed(
            "invalid feedback capability response: expected an object; no transition was sent",
        ));
    }
    let capability: Capability = serde_json::from_value(capability)
        .map_err(|_| failed("invalid feedback capability response; no transition was sent"))?;
    if capability.schema_version == 1 && capability.feedback_transition_v1 == 0 {
        context.code = oak_core::ErrorCode::FeedbackTransitionUnavailable;
        return Err(failed("feedback_transition_v1 is not advertised by this server; no transition was sent. The capability may be disabled or not yet supported. Use the existing authorized feedback web interface, or ask the server operator about rollout. This command does not activate the feature or fall back to legacy writes."));
    }
    if capability.schema_version != 1 || capability.feedback_transition_v1 != 1 {
        context.code = oak_core::ErrorCode::FeedbackCapabilityUnsupported;
        return Err(failed(
            "unsupported feedback capability version; no transition was sent. Check client/server compatibility with the server operator",
        ));
    }
    context.code = oak_core::ErrorCode::FeedbackTransitionPreflightFailed;
    let observed = inventory::resolve_item(&api, &options.item).await?;
    let mut body =
        serde_json::json!({"protocol":PROTOCOL,"expected_revision":options.expected_revision});
    if let Some(status) = &options.status {
        body["status"] = serde_json::json!(status);
    }
    if options.clear_notes {
        body["admin_notes"] = serde_json::Value::Null;
    }
    if let Some(notes) = &options.notes {
        body["admin_notes"] = serde_json::json!(notes);
    }
    let encoded = serde_json::to_vec(&body).map_err(|_| usage("invalid transition payload"))?;
    if encoded.len() > 128 * 1024 {
        return Err(usage(
            "transition payload exceeds the server's 128 KiB limit",
        ));
    }
    let request = api
        .authed(crate::http::api_client().post(format!(
            "{}/api/feedback/{}/transition",
            api.remote,
            urlencoding::encode(&observed.id)
        )))
        .timeout(TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(encoded);
    // All failures after send are interpreted without printing untrusted bodies
    // or request values (which may contain private notes or a credential echo).
    // This records that send was attempted, not proof the server received it.
    // Any ambiguous failure from here on must never claim no mutation was sent.
    context.transition_sent = true;
    context.outcome = "unconfirmed";
    context.code = oak_core::ErrorCode::FeedbackTransitionUnconfirmed;
    let response = request.send().await.map_err(|_| unconfirmed(&refresh))?;
    let status = response.status();
    if status == reqwest::StatusCode::PRECONDITION_FAILED {
        context.outcome = "rejected";
        context.code = oak_core::ErrorCode::RevisionConflict;
        return Err(failed(format!("revision_conflict: the ticket changed. Refresh with `{refresh}` and reconcile before issuing a new transition; the supplied revision was not replaced or retried.")));
    }
    if status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT {
        context.outcome = "rejected";
        context.code = oak_core::ErrorCode::FeedbackTransitionRejected;
        return Err(failed(format!("feedback transition rejected: HTTP {}; review permissions, activation and arguments before retrying",status.as_u16())));
    }
    if status != reqwest::StatusCode::OK {
        return Err(unconfirmed(&refresh));
    }
    let receipt: WireReceipt = bounded_json(response)
        .await
        .map_err(|_| unconfirmed(&refresh))?;
    let requested_notes = if options.clear_notes || options.notes.as_deref() == Some("") {
        Some(serde_json::Value::Null)
    } else {
        options.notes.as_ref().map(|notes| serde_json::json!(notes))
    };
    if receipt.schema_version != 1
        || receipt.protocol != PROTOCOL
        || receipt.item.id != observed.id
        || receipt.item.number != observed.number
        || !inventory::valid_item(&receipt.item)
        || !inventory::valid_revision(&receipt.revision)
        || receipt.previous_revision != options.expected_revision
        || receipt.item.revision.as_deref() != Some(receipt.revision.as_str())
        || receipt.changed == (receipt.revision == receipt.previous_revision)
        || options
            .status
            .as_ref()
            .is_some_and(|status| status != &receipt.item.status)
        || requested_notes
            .as_ref()
            .is_some_and(|notes| notes != &receipt.admin_notes)
    {
        return Err(unconfirmed(&refresh));
    }
    let receipt = Receipt {
        schema_version: 1,
        protocol: PROTOCOL,
        confirmed: true,
        changed: receipt.changed,
        feedback_id: receipt.item.id,
        feedback_ref: format!("fb-{}", receipt.item.number),
        status: receipt.item.status,
        previous_revision: receipt.previous_revision,
        revision: receipt.revision,
    };
    context.outcome = "confirmed";
    context.code = oak_core::ErrorCode::FeedbackTransitionOutputFailed;
    if options.json {
        output::print_json(&receipt).map_err(failed)?;
    } else {
        output::print_line(&format!(
            "{}: {} ({})\nRevision: {}",
            receipt.feedback_ref,
            receipt.status,
            if receipt.changed {
                "updated"
            } else {
                "unchanged"
            },
            receipt.revision
        ));
    }
    Ok(())
}
