//! `oak auth status --json` — read-only account/credential diagnosis (fb-480).
//!
//! Reports which credential the CLI would send to a remote (by *source*,
//! never the secret), what identity the server resolves it to, and whether
//! the feedback-admin surface answers. The admin routes deliberately answer
//! non-admins with a quiet 404, which is indistinguishable from "not
//! deployed", so that probe reports `absent_or_denied` rather than guessing.

use std::path::Path;

use oak_core::{MetadataKey, Result};
use serde::{Deserialize, Serialize};

use crate::output;

/// Bound on any probe response body we read.
const PROBE_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
pub struct AuthStatusJson {
    pub schema_version: u32,
    pub remote: String,
    /// `argument`, `env` (OAK_REMOTE), `repository` (this checkout's
    /// remote), or `default`.
    pub remote_source: String,
    pub credential: CredentialJson,
    pub identity: IdentityJson,
    pub capabilities: CapabilitiesJson,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recommended_next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CredentialJson {
    /// `env` (OAK_API_KEY), `repository` (this checkout's stored key),
    /// `credentials_file` (~/.oak/credentials), or `none`. Precedence is
    /// env, then repository key, then credentials file, but the repository
    /// key is only considered when this checkout's remote is the queried
    /// remote (some repository-aware commands do not apply that scoping).
    pub source: &'static str,
    pub present: bool,
    /// The username recorded next to the stored login for this remote, if
    /// any. Informational; `identity` is what the server actually resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stored_username: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IdentityJson {
    /// `authenticated`, `unauthenticated` (no credential, or the server
    /// rejected it), `unreachable` (no reply: could not connect, or timed
    /// out — see `detail`), or `unknown` (unexpected reply).
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CapabilitiesJson {
    /// `available`, `absent_or_denied` (quiet 404/405: either not deployed
    /// or not an admin — the server does not say which),
    /// `unauthenticated`, `unreachable`, or `unknown`.
    pub feedback_admin: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_admin_http_status: Option<u16>,
}

#[derive(Deserialize)]
struct WhoamiWire {
    username: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// Where the credential for `remote` would come from: env, then the
/// repository key (the caller passes it only when the checkout is linked to
/// `remote`), then the stored login for `remote`.
fn credential_source(
    remote: &str,
    repository_token: Option<String>,
) -> (&'static str, Option<String>) {
    let non_blank = |token: Option<String>| {
        token
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    };
    if let Some(token) = non_blank(std::env::var("OAK_API_KEY").ok()) {
        return ("env", Some(token));
    }
    if let Some(token) = non_blank(repository_token) {
        return ("repository", Some(token));
    }
    if let Some(token) = non_blank(super::credentials::get_token_for_server(remote)) {
        return ("credentials_file", Some(token));
    }
    ("none", None)
}

async fn probe(url: String, token: Option<&str>) -> std::result::Result<(u16, Vec<u8>), String> {
    let client = crate::http::api_client();
    let timeout = crate::http::read_only_probe_timeout();
    let mut request = client.get(url).timeout(timeout);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    // Transport diagnostics can echo URLs; keep only a coarse reason.
    let mut response = request
        .send()
        .await
        .map_err(|error| crate::http::transport_failure_reason(&error, timeout))?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if chunk.len() > PROBE_MAX_BYTES.saturating_sub(body.len()) {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

/// Resolve the remote for `oak auth status`: explicit/env first, then the
/// checkout's linked remote, then the default.
pub fn resolve_remote(
    explicit_or_env: Option<(String, &'static str)>,
    cwd: &Path,
) -> (String, String) {
    if let Some((remote, source)) = explicit_or_env {
        return (remote, source.to_string());
    }
    if let Some(remote) = repository_metadata(cwd, MetadataKey::RemoteUrl) {
        return (
            remote.trim_end_matches('/').to_string(),
            "repository".to_string(),
        );
    }
    (
        super::credentials::DEFAULT_REMOTE.to_string(),
        "default".to_string(),
    )
}

fn repository_metadata(cwd: &Path, key: MetadataKey) -> Option<String> {
    let ctx = crate::resolve::resolve(cwd).ok()?;
    let repo = ctx.open().ok()?;
    repo.get_metadata(key).ok().flatten()
}

/// Gather the status. Never errors on remote failures: those are reported
/// as states, so the command always yields one JSON document.
pub async fn status(remote: &str, remote_source: &str, cwd: &Path) -> Result<AuthStatusJson> {
    let remote = remote.trim_end_matches('/').to_string();
    // A repository key only applies to the origin that issued it (its
    // recorded `ApiKeyOrigin`, not whatever `RemoteUrl` currently says) —
    // the same scoping every network command uses.
    let repository_token = crate::resolve::resolve(cwd)
        .ok()
        .and_then(|ctx| ctx.open().ok())
        .and_then(|repo| super::credentials::repository_credential(repo.as_ref()))
        .and_then(|credential| credential.token_for(&remote));
    let (source, token) = credential_source(&remote, repository_token);
    let credential = CredentialJson {
        source,
        present: token.is_some(),
        stored_username: super::credentials::get_username_for_server(&remote),
    };

    let identity = match probe(format!("{remote}/api/whoami"), token.as_deref()).await {
        Ok((200, body)) => match serde_json::from_slice::<WhoamiWire>(&body) {
            Ok(who) => IdentityJson {
                state: "authenticated",
                http_status: Some(200),
                username: Some(who.username),
                display_name: who.display_name,
                detail: None,
            },
            Err(_) => IdentityJson {
                state: "unknown",
                http_status: Some(200),
                username: None,
                display_name: None,
                detail: Some("the server's identity reply was unreadable".to_string()),
            },
        },
        Ok((status @ (401 | 403), _)) => IdentityJson {
            state: "unauthenticated",
            http_status: Some(status),
            username: None,
            display_name: None,
            detail: Some(if token.is_some() {
                "the server rejected the credential".to_string()
            } else {
                "no credential was sent".to_string()
            }),
        },
        Ok((status, _)) => IdentityJson {
            state: "unknown",
            http_status: Some(status),
            username: None,
            display_name: None,
            detail: Some(format!(
                "unexpected HTTP {status} from the identity endpoint"
            )),
        },
        Err(reason) => IdentityJson {
            state: "unreachable",
            http_status: None,
            username: None,
            display_name: None,
            detail: Some(reason),
        },
    };

    let capabilities = if identity.state == "unreachable" {
        CapabilitiesJson {
            feedback_admin: "unreachable",
            feedback_admin_http_status: None,
        }
    } else {
        match probe(
            format!("{remote}/api/feedback/capabilities"),
            token.as_deref(),
        )
        .await
        {
            Ok((status, _)) => CapabilitiesJson {
                feedback_admin: match status {
                    200 => "available",
                    404 | 405 => "absent_or_denied",
                    401 => "unauthenticated",
                    _ => "unknown",
                },
                feedback_admin_http_status: Some(status),
            },
            Err(_) => CapabilitiesJson {
                feedback_admin: "unreachable",
                feedback_admin_http_status: None,
            },
        }
    };

    // Requests used the remote as given; everything printed drops userinfo.
    let remote = crate::http::redact_url_userinfo(&remote);
    let mut recommended_next_commands = Vec::new();
    if identity.state == "unauthenticated" {
        recommended_next_commands.push(format!(
            "oak login --remote={}",
            output::shell_quote(&remote)
        ));
    }

    Ok(AuthStatusJson {
        schema_version: crate::work_state::SCHEMA_VERSION,
        remote,
        remote_source: remote_source.to_string(),
        credential,
        identity,
        capabilities,
        recommended_next_commands,
    })
}

/// Human-readable rendering of [`status`].
pub fn print_human(status: &AuthStatusJson) {
    output::print_line(&format!(
        "Remote: {} ({})",
        status.remote, status.remote_source
    ));
    output::print_line(&format!(
        "Credential: {}",
        match status.credential.source {
            "env" => "OAK_API_KEY environment variable",
            "repository" => "this repository's stored key",
            "credentials_file" => "~/.oak/credentials",
            _ => "none",
        }
    ));
    match (&status.identity.username, status.identity.state) {
        (Some(username), _) => output::print_line(&format!("Identity: {username}")),
        (None, state) => output::print_line(&format!("Identity: {state}")),
    }
    output::print_line(&format!(
        "Feedback admin: {}",
        status.capabilities.feedback_admin
    ));
    for command in &status.recommended_next_commands {
        output::print_line(&format!("Next: {command}"));
    }
}
