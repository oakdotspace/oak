//! `oak site` — manage Oak Pages sites (`https://<name>.oak.page/`).
//!
//! A site is a globally unique name pointing at a folder of a repo's `main`
//! branch; one repo can have many sites. Every subcommand is repo-scoped
//! (`/api/{owner}/{repo}/pages/sites...`): the repo defaults to the cwd's
//! linked checkout and `--repo OWNER/NAME` overrides it. `oak site list
//! --org ORG` lists across an organization, and `oak site check NAME` asks
//! whether a name is free (`/api/pages/names/{name}`).
//!
//! `--json` prints the server's JSON document unchanged. Name syntax is
//! validated here with the server's rule so a typo fails before any request;
//! reserved names are only known server-side.

use std::path::Path;

use oak_core::{OakError, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::output;

use super::credentials::effective_token_for_worktree;

/// Domain sites are served under. Only used where no server `url` is at hand
/// (`oak open --site NAME`); everything else prints the server's `url`.
pub const PAGES_DOMAIN: &str = "oak.page";

/// The URL a site is served at, built without asking the server.
pub fn site_url(name: &str) -> String {
    format!("https://{name}.{PAGES_DOMAIN}/")
}

/// Validate a site name with the server's syntax rule: a DNS label of
/// lowercase letters, digits and inner hyphens
/// (`^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`), and no `--` at characters 3–4
/// (the IDNA `xn--` shape). Reserved names are checked server-side only.
pub fn validate_site_name(name: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(OakError::InvalidArgument(format!(
            "invalid site name '{name}': {why}"
        )))
    };
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return invalid("must be 1-63 characters");
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return invalid("use only lowercase letters, digits and '-'");
    }
    if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return invalid("cannot start or end with '-'");
    }
    if bytes.len() >= 4 && &bytes[2..4] == b"--" {
        return invalid("cannot have '--' at characters 3-4");
    }
    Ok(())
}

fn validate_org_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug == "." || slug == ".." {
        return Err(OakError::InvalidArgument(
            "organization must be a non-empty path segment".to_string(),
        ));
    }
    if slug
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
    {
        return Err(OakError::InvalidArgument(
            "organization cannot contain whitespace, control characters, or path separators"
                .to_string(),
        ));
    }
    Ok(())
}

/// Basic-auth change for `oak site set`.
#[derive(Debug, PartialEq, Eq)]
pub enum PasswordChange {
    Set { username: String, password: String },
    Remove,
}

/// What `oak site` was asked to do, already parsed from the command line.
#[derive(Debug)]
pub enum SiteAction {
    List {
        org: Option<String>,
    },
    Create {
        name: String,
        dir: Option<String>,
        spa: bool,
    },
    Show {
        name: String,
    },
    Rename {
        name: String,
        new_name: String,
    },
    Set {
        name: String,
        dir: Option<String>,
        spa: Option<bool>,
        password: Option<PasswordChange>,
    },
    Rm {
        name: String,
    },
    Deploys {
        name: String,
    },
    Redeploy {
        name: String,
    },
    Rollback {
        name: String,
        to: Option<i64>,
    },
    Check {
        name: String,
    },
    DomainList {
        name: String,
    },
    DomainAdd {
        name: String,
        domain: String,
    },
    DomainVerify {
        name: String,
        domain: String,
    },
    DomainPrimary {
        name: String,
        domain: String,
        primary: bool,
    },
    DomainRm {
        name: String,
        domain: String,
    },
}

impl SiteAction {
    /// Every site name this action carries, validated before any request.
    fn names(&self) -> Vec<&str> {
        match self {
            SiteAction::List { .. } => vec![],
            SiteAction::Rename { name, new_name } => vec![name, new_name],
            SiteAction::Create { name, .. }
            | SiteAction::Show { name }
            | SiteAction::Set { name, .. }
            | SiteAction::Rm { name }
            | SiteAction::Deploys { name }
            | SiteAction::Redeploy { name }
            | SiteAction::Rollback { name, .. }
            | SiteAction::Check { name }
            | SiteAction::DomainList { name }
            | SiteAction::DomainAdd { name, .. }
            | SiteAction::DomainVerify { name, .. }
            | SiteAction::DomainPrimary { name, .. }
            | SiteAction::DomainRm { name, .. } => vec![name],
        }
    }

    /// The custom domain this action names, if any.
    fn domain(&self) -> Option<&str> {
        match self {
            SiteAction::DomainAdd { domain, .. }
            | SiteAction::DomainVerify { domain, .. }
            | SiteAction::DomainPrimary { domain, .. }
            | SiteAction::DomainRm { domain, .. } => Some(domain),
            _ => None,
        }
    }

    /// Whether the action addresses a repo (everything but `check` and
    /// `list --org`).
    fn needs_repo(&self) -> bool {
        !matches!(
            self,
            SiteAction::Check { .. } | SiteAction::List { org: Some(_) }
        )
    }
}

/// Body of `PATCH .../sites/{name}` for `oak site set`. `None` leaves the
/// field out; a password removal sends `"basic_auth": null`.
pub fn set_body(
    dir: Option<&str>,
    spa: Option<bool>,
    password: Option<&PasswordChange>,
) -> Result<Value> {
    let mut body = serde_json::Map::new();
    if let Some(dir) = dir {
        body.insert("source_dir".into(), Value::from(dir));
    }
    if let Some(spa) = spa {
        body.insert("spa_fallback".into(), Value::from(spa));
    }
    match password {
        Some(PasswordChange::Set { username, password }) => {
            body.insert(
                "basic_auth".into(),
                serde_json::json!({ "username": username, "password": password }),
            );
        }
        Some(PasswordChange::Remove) => {
            body.insert("basic_auth".into(), Value::Null);
        }
        None => {}
    }
    if body.is_empty() {
        return Err(OakError::InvalidArgument(
            "nothing to change: pass --dir, --spa/--no-spa, or --password-user/--password-file/--no-password"
                .to_string(),
        ));
    }
    Ok(Value::Object(body))
}

/// Read a basic-auth password from a file (`-` for stdin), dropping one
/// trailing newline so `echo secret > f` works.
pub fn read_password_file(path: &Path) -> Result<String> {
    let mut text = if path == Path::new("-") {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            OakError::InvalidArgument(format!("cannot read password file {}: {e}", path.display()))
        })?
    };
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    if text.is_empty() {
        return Err(OakError::InvalidArgument(
            "password file is empty".to_string(),
        ));
    }
    Ok(text)
}

/// Normalize a custom domain the way the server stores it (lowercase, no
/// trailing dot) and catch obvious mistakes before a request. The server
/// has the full rules.
pub fn normalize_domain(input: &str) -> Result<String> {
    let d = input.trim().trim_end_matches('.').to_ascii_lowercase();
    let invalid = |why: &str| {
        Err(OakError::InvalidArgument(format!(
            "invalid domain '{}': {why}",
            input.trim()
        )))
    };
    if d.contains("://") || d.contains('/') {
        return invalid("give just the domain, without https:// or a path");
    }
    if d.is_empty() || !d.contains('.') {
        return invalid("give a full domain, like www.example.com");
    }
    if d.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return invalid("domains can't contain spaces");
    }
    Ok(d)
}

// ---------------------------------------------------------------------------
// Wire types (lenient: unknown fields are ignored, missing ones default).

#[derive(Deserialize, Default)]
#[serde(default)]
struct SiteWire {
    name: String,
    url: String,
    repo: String,
    source_dir: String,
    spa_fallback: bool,
    password_protected: bool,
    disabled: bool,
    disabled_reason: Option<String>,
    deployment: Option<DeploymentWire>,
    updated_at: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeploymentWire {
    id: i64,
    status: String,
    failure: Option<String>,
    commit: Option<String>,
    file_count: Option<i64>,
    total_bytes: Option<i64>,
    origin: String,
    created_by: Option<String>,
    created_at: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DnsRecordWire {
    #[serde(rename = "type")]
    rtype: String,
    name: String,
    value: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DnsStatusWire {
    pointed: Option<bool>,
    records: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DomainWire {
    domain: String,
    url: String,
    status: String,
    primary: bool,
    verification: DnsRecordWire,
    routing: DnsRecordWire,
    certificate: Option<String>,
    error: Option<String>,
    failing_since: Option<String>,
    dns: Option<DnsStatusWire>,
    edge_error: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DomainListWire {
    domains: Vec<DomainWire>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SiteListWire {
    sites: Vec<SiteWire>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeploymentListWire {
    deployments: Vec<DeploymentWire>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NameCheckWire {
    name: String,
    available: bool,
    reason: Option<String>,
}

fn decode<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T> {
    T::deserialize(value).map_err(|e| {
        OakError::Server(format!(
            "the server returned an unreadable Pages response: {e}"
        ))
    })
}

// ---------------------------------------------------------------------------
// HTTP

struct Api {
    remote: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl Api {
    fn new(work_path: &Path, remote: Option<&str>) -> Self {
        let remote = super::repo_visibility::resolve_remote(work_path, remote);
        let token = effective_token_for_worktree(&remote, work_path);
        Self {
            remote,
            token,
            client: crate::http::api_client(),
        }
    }

    /// Send a request and return the JSON body (`Value::Null` for an empty
    /// 2xx such as DELETE's 204). Non-2xx becomes an error carrying the
    /// server's `{"error": "..."}` message.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.remote));
        if let Some(body) = body {
            req = req.json(body);
        }
        if let Some(token) = &self.token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(OakError::AuthRequired {
                remote: crate::http::redact_url_userinfo(&self.remote),
                status: 401,
            });
        }
        if !status.is_success() {
            return Err(crate::http::server_error(resp).await);
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| OakError::Http(e.to_string()))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            OakError::Server(format!(
                "the server returned a non-JSON response (HTTP {status})"
            ))
        })
    }
}

/// Run one `oak site` action. Returns the process exit code: 0 on success,
/// 1 when a deploy came back `failed` or `check` found the name taken.
pub async fn run(
    work_path: &Path,
    repo_arg: Option<&str>,
    remote_arg: Option<&str>,
    json: bool,
    action: SiteAction,
) -> Result<i32> {
    for name in action.names() {
        validate_site_name(name)?;
    }
    let domain = action.domain().map(normalize_domain).transpose()?;
    if let SiteAction::List { org: Some(org) } = &action {
        validate_org_slug(org)?;
    }
    let repo_path = if action.needs_repo() {
        let (owner, name) = super::repo_visibility::resolve_repo(work_path, repo_arg)?;
        format!("/api/{owner}/{name}/pages/sites")
    } else {
        String::new()
    };
    let api = Api::new(work_path, remote_arg);
    use reqwest::Method;

    let emit = |value: &Value| -> Result<()> {
        if json {
            output::print_json(value)?;
        }
        Ok(())
    };

    match action {
        SiteAction::List { org } => {
            let path = match &org {
                Some(org) => format!("/api/orgs/{org}/pages/sites"),
                None => repo_path,
            };
            let value = api.send(Method::GET, &path, None).await?;
            emit(&value)?;
            if !json {
                print_site_list(&decode::<SiteListWire>(&value)?);
            }
        }
        SiteAction::Create { name, dir, spa } => {
            let body = serde_json::json!({
                "name": name,
                "source_dir": dir.as_deref().unwrap_or("/"),
                "spa_fallback": spa,
            });
            let value = api.send(Method::POST, &repo_path, Some(&body)).await?;
            emit(&value)?;
            if !json {
                let site: SiteWire = decode(&value)?;
                output::success(&format!("Created site {} → {}", site.name, site.url));
                if let Some(d) = &site.deployment {
                    output::print_line(&format!("  deployment: {}", deployment_line(d)));
                }
            }
        }
        SiteAction::Show { name } => {
            let value = api
                .send(Method::GET, &format!("{repo_path}/{name}"), None)
                .await?;
            emit(&value)?;
            if !json {
                print_site(&decode(&value)?);
            }
        }
        SiteAction::Rename { name, new_name } => {
            let body = serde_json::json!({ "name": new_name });
            let value = api
                .send(Method::PATCH, &format!("{repo_path}/{name}"), Some(&body))
                .await?;
            emit(&value)?;
            if !json {
                let site: SiteWire = decode(&value)?;
                output::success(&format!("Renamed {name} → {} ({})", site.name, site.url));
            }
        }
        SiteAction::Set {
            name,
            dir,
            spa,
            password,
        } => {
            let body = set_body(dir.as_deref(), spa, password.as_ref())?;
            let value = api
                .send(Method::PATCH, &format!("{repo_path}/{name}"), Some(&body))
                .await?;
            emit(&value)?;
            if !json {
                let site: SiteWire = decode(&value)?;
                output::success(&format!("Updated site {} → {}", site.name, site.url));
            }
        }
        SiteAction::Rm { name } => {
            api.send(Method::DELETE, &format!("{repo_path}/{name}"), None)
                .await?;
            if json {
                // DELETE answers 204 with no body; report the outcome.
                output::print_json(&serde_json::json!({ "name": name, "deleted": true }))?;
            } else {
                output::success(&format!("Deleted site {name}"));
            }
        }
        SiteAction::Deploys { name } => {
            let value = api
                .send(
                    Method::GET,
                    &format!("{repo_path}/{name}/deployments"),
                    None,
                )
                .await?;
            emit(&value)?;
            if !json {
                let list: DeploymentListWire = decode(&value)?;
                if list.deployments.is_empty() {
                    output::info(&format!("No deployments for {name} yet."));
                }
                for d in &list.deployments {
                    output::print_line(&deployment_line(d));
                }
            }
        }
        SiteAction::Redeploy { name } => {
            let body = serde_json::json!({ "action": "redeploy" });
            return deploy(
                &api,
                &format!("{repo_path}/{name}/deployments"),
                &body,
                json,
            )
            .await;
        }
        SiteAction::Rollback { name, to } => {
            let mut body = serde_json::json!({ "action": "rollback" });
            if let Some(to) = to {
                body["to"] = Value::from(to);
            }
            return deploy(
                &api,
                &format!("{repo_path}/{name}/deployments"),
                &body,
                json,
            )
            .await;
        }
        SiteAction::Check { name } => {
            let value = api
                .send(Method::GET, &format!("/api/pages/names/{name}"), None)
                .await?;
            emit(&value)?;
            let check: NameCheckWire = decode(&value)?;
            if !json {
                if check.available {
                    output::success(&format!(
                        "{} is available ({})",
                        check.name,
                        site_url(&check.name)
                    ));
                } else {
                    let reason = check.reason.as_deref().unwrap_or("taken");
                    output::warning(&format!("{} is not available: {reason}", check.name));
                }
            }
            return Ok(if check.available { 0 } else { 1 });
        }
        SiteAction::DomainList { name } => {
            let value = api
                .send(Method::GET, &format!("{repo_path}/{name}/domains"), None)
                .await?;
            emit(&value)?;
            if !json {
                let list: DomainListWire = decode(&value)?;
                if list.domains.is_empty() {
                    output::info(&format!(
                        "{name} has no custom domains. Add one with `oak site domain add {name} DOMAIN`."
                    ));
                }
                for d in &list.domains {
                    print_domain(d);
                }
            }
        }
        SiteAction::DomainAdd { name, .. } => {
            let domain = domain.unwrap_or_default();
            let body = serde_json::json!({ "domain": domain });
            let value = api
                .send(
                    Method::POST,
                    &format!("{repo_path}/{name}/domains"),
                    Some(&body),
                )
                .await?;
            emit(&value)?;
            if !json {
                let d: DomainWire = decode(&value)?;
                if d.status == "verified" {
                    output::success(&format!("{} already serves {name}", d.domain));
                } else {
                    output::success(&format!("Added {} to {name}", d.domain));
                    output::print_line("Create these DNS records, then run:");
                    output::print_line(&format!("  oak site domain verify {name} {}", d.domain));
                }
                print_domain(&d);
            }
        }
        SiteAction::DomainVerify { name, .. } => {
            let domain = domain.unwrap_or_default();
            let value = match api
                .send(
                    Method::POST,
                    &format!("{repo_path}/{name}/domains/{domain}/verify"),
                    None,
                )
                .await
            {
                Ok(v) => v,
                // The record isn't there yet: say what was found, exit 1.
                Err(OakError::Server(msg)) if !json => {
                    output::error(&msg);
                    return Ok(1);
                }
                Err(e) => return Err(e),
            };
            emit(&value)?;
            if !json {
                let d: DomainWire = decode(&value)?;
                output::success(&format!("{} is verified and serves {name}", d.domain));
                print_domain(&d);
            }
        }
        SiteAction::DomainPrimary { name, primary, .. } => {
            let domain = domain.unwrap_or_default();
            let body = serde_json::json!({ "primary": primary });
            let value = api
                .send(
                    Method::PATCH,
                    &format!("{repo_path}/{name}/domains/{domain}"),
                    Some(&body),
                )
                .await?;
            emit(&value)?;
            if !json {
                if primary {
                    output::success(&format!(
                        "{name}'s other addresses now redirect to https://{domain}/"
                    ));
                } else {
                    output::success(&format!("{domain} is no longer {name}'s primary domain"));
                }
            }
        }
        SiteAction::DomainRm { name, .. } => {
            let domain = domain.unwrap_or_default();
            api.send(
                Method::DELETE,
                &format!("{repo_path}/{name}/domains/{domain}"),
                None,
            )
            .await?;
            if json {
                output::print_json(&serde_json::json!({
                    "name": name,
                    "domain": domain,
                    "removed": true,
                }))?;
            } else {
                output::success(&format!("Removed {domain} from {name}"));
            }
        }
    }
    Ok(0)
}

fn print_domain(d: &DomainWire) {
    let mut state = d.status.clone();
    if d.primary {
        state.push_str(", primary");
    }
    if let Some(cert) = &d.certificate {
        if cert != "active" {
            state.push_str(&format!(", certificate {}", cert.replace('_', " ")));
        }
    }
    output::print_line(&format!("{}  {}  [{state}]", d.domain, d.url));
    if d.status != "verified" || d.failing_since.is_some() {
        output::print_line(&format!(
            "  {:<5} {}  {}",
            d.verification.rtype, d.verification.name, d.verification.value
        ));
    }
    let pointed = d.dns.as_ref().and_then(|s| s.pointed);
    if d.status != "verified" || pointed == Some(false) {
        output::print_line(&format!(
            "  {:<5} {}  {}   (apex domains: ALIAS/ANAME or flattened CNAME)",
            d.routing.rtype, d.routing.name, d.routing.value
        ));
    }
    if let Some(dns) = &d.dns {
        match dns.pointed {
            Some(true) => output::print_line("  DNS points at Oak."),
            Some(false) if dns.records.is_empty() => {
                output::print_line("  DNS: no records found for this domain yet.")
            }
            Some(false) => output::print_line(&format!(
                "  DNS points elsewhere: {}",
                dns.records.join(", ")
            )),
            None => {}
        }
    }
    if let Some(e) = &d.error {
        output::print_line(&format!("  {e}"));
    }
    if let Some(e) = &d.edge_error {
        output::print_line(&format!("  certificate request failed, retrying: {e}"));
    }
}

/// POST a redeploy/rollback. A deploy the server ran but that failed comes
/// back 200 with `status: "failed"`: show it and exit 1.
async fn deploy(api: &Api, path: &str, body: &Value, json: bool) -> Result<i32> {
    let value = api.send(reqwest::Method::POST, path, Some(body)).await?;
    if json {
        output::print_json(&value)?;
    }
    let d: DeploymentWire = decode(&value)?;
    if d.status == "failed" {
        output::error(&format!(
            "Deployment #{} failed: {}",
            d.id,
            d.failure.as_deref().unwrap_or("no reason given")
        ));
        return Ok(1);
    }
    if !json {
        output::success(&format!("Deployment {}", deployment_line(&d)));
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Human output

fn short_commit(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn human_bytes(n: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn deployment_line(d: &DeploymentWire) -> String {
    let mut line = format!("#{} {} ({})", d.id, d.status, d.origin);
    if let Some(commit) = &d.commit {
        line.push_str(&format!(" {}", short_commit(commit)));
    }
    if let Some(files) = d.file_count {
        line.push_str(&format!(", {files} files"));
    }
    if let Some(bytes) = d.total_bytes {
        line.push_str(&format!(", {}", human_bytes(bytes)));
    }
    if !d.created_at.is_empty() {
        line.push_str(&format!(", {}", d.created_at));
    }
    if let Some(by) = &d.created_by {
        line.push_str(&format!(" by {by}"));
    }
    if let Some(failure) = &d.failure {
        line.push_str(&format!(" — {failure}"));
    }
    line
}

fn site_flags(site: &SiteWire) -> String {
    let mut flags = Vec::new();
    if site.spa_fallback {
        flags.push("spa".to_string());
    }
    if site.password_protected {
        flags.push("password".to_string());
    }
    if site.disabled {
        flags.push(match &site.disabled_reason {
            Some(reason) => format!("disabled: {reason}"),
            None => "disabled".to_string(),
        });
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    }
}

fn print_site_list(list: &SiteListWire) {
    if list.sites.is_empty() {
        output::info("No sites. Create one with `oak site create NAME`.");
        return;
    }
    for s in &list.sites {
        output::print_line(&format!(
            "{}  {}  {}:{}{}",
            s.name,
            s.url,
            s.repo,
            s.source_dir,
            site_flags(s)
        ));
    }
}

fn print_site(site: &SiteWire) {
    output::print_line(&format!("{}  {}{}", site.name, site.url, site_flags(site)));
    output::print_line(&format!("  repo:       {}", site.repo));
    output::print_line(&format!("  source dir: {}", site.source_dir));
    match &site.deployment {
        Some(d) => output::print_line(&format!("  deployment: {}", deployment_line(d))),
        None => output::print_line("  deployment: none"),
    }
    output::print_line(&format!("  updated:    {}", site.updated_at));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_names_matching_the_server_rule_are_accepted() {
        let max = "a".repeat(63);
        for name in [
            "a", "0", "docs", "my-site", "a1-b2", "ab-c--d", "0123", &max,
        ] {
            validate_site_name(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        // `--` is only forbidden at characters 3-4.
        validate_site_name("abc--d").unwrap();
        validate_site_name("a--").unwrap_err();
    }

    #[test]
    fn site_names_breaking_the_server_rule_are_usage_errors() {
        let too_long = "a".repeat(64);
        for name in [
            "", "-docs", "docs-", "-", "Docs", "my_site", "my.site", "my site", "xn--abc",
            "ab--cd", "é", &too_long,
        ] {
            let err = validate_site_name(name).unwrap_err();
            assert!(
                matches!(err, OakError::InvalidArgument(_)),
                "{name:?} should be InvalidArgument, got {err:?}"
            );
        }
    }

    #[test]
    fn site_url_uses_the_pages_domain() {
        assert_eq!(site_url("docs"), "https://docs.oak.page/");
    }

    #[test]
    fn set_body_sends_only_the_fields_asked_for() {
        assert_eq!(
            set_body(Some("public"), None, None).unwrap(),
            serde_json::json!({ "source_dir": "public" })
        );
        assert_eq!(
            set_body(None, Some(false), Some(&PasswordChange::Remove)).unwrap(),
            serde_json::json!({ "spa_fallback": false, "basic_auth": null })
        );
        let set = PasswordChange::Set {
            username: "u".into(),
            password: "p".into(),
        };
        assert_eq!(
            set_body(None, None, Some(&set)).unwrap(),
            serde_json::json!({ "basic_auth": { "username": "u", "password": "p" } })
        );
        assert!(matches!(
            set_body(None, None, None).unwrap_err(),
            OakError::InvalidArgument(_)
        ));
    }

    #[test]
    fn password_file_drops_one_trailing_newline() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("pw");
        std::fs::write(&path, "s3cret\n").unwrap();
        assert_eq!(read_password_file(&path).unwrap(), "s3cret");
        std::fs::write(&path, "\n").unwrap();
        assert!(read_password_file(&path).is_err());
    }

    #[test]
    fn domains_are_normalized_before_any_request() {
        assert_eq!(
            normalize_domain(" WWW.Example.com. ").unwrap(),
            "www.example.com"
        );
        for bad in [
            "",
            "example",
            "https://example.com",
            "example.com/x",
            "a b.com",
        ] {
            assert!(
                matches!(normalize_domain(bad), Err(OakError::InvalidArgument(_))),
                "{bad:?}"
            );
        }
        let add = SiteAction::DomainAdd {
            name: "docs".into(),
            domain: "x.com".into(),
        };
        assert_eq!(add.names(), vec!["docs"]);
        assert_eq!(add.domain(), Some("x.com"));
        assert!(add.needs_repo());
    }

    #[test]
    fn check_and_org_list_do_not_need_a_repo() {
        assert!(!SiteAction::Check { name: "x".into() }.needs_repo());
        assert!(!SiteAction::List {
            org: Some("acme".into())
        }
        .needs_repo());
        assert!(SiteAction::List { org: None }.needs_repo());
        assert!(SiteAction::Show { name: "x".into() }.needs_repo());
    }

    #[test]
    fn invalid_name_fails_before_resolving_the_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(run(
                temp.path(),
                None,
                None,
                false,
                SiteAction::Show {
                    name: "Bad_Name".into(),
                },
            ))
            .unwrap_err();
        assert!(err.to_string().contains("invalid site name"), "{err}");
    }

    #[test]
    fn deployment_line_shows_failure_and_short_commit() {
        let d = DeploymentWire {
            id: 7,
            status: "failed".into(),
            failure: Some("source_dir not found".into()),
            commit: Some("0123456789abcdef0123".into()),
            file_count: None,
            total_bytes: None,
            origin: "manual".into(),
            created_by: None,
            created_at: String::new(),
        };
        assert_eq!(
            deployment_line(&d),
            "#7 failed (manual) 0123456789ab — source_dir not found"
        );
    }
}
