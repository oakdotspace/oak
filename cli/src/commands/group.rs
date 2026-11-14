//! `oak group` and `oak groups`: an org's groups (`/api/orgs/:org/groups`).
//!
//! A group is a list of people (and the agents they make keys for) that repo
//! access and path rules are granted to. Give a group Read or Write on a repo
//! (`oak group grant`) and everyone in it gets that; a path rule names it as
//! `@group/web`. Groups can be nested: a member of a sub-group counts as a
//! member of every group above it, and a group's maintainers run it and every
//! group below it. Org owners and admins run them all.
//!
//! Every org has two built-in groups: `public` (anyone, signed in or not;
//! granting it Read makes a repo public) and `members` (everyone in the org:
//! adding someone to it adds them to the org, removing them takes them out).
//!
//! An org's own agents (service accounts) are on groups too: `oak group agent
//! add web deploy-bot` puts the agent `deploy-bot` on `web`, and its keys then
//! reach what `web` reaches, never its creator's access. `oak group agent list`
//! lists the org's agents and their groups.
//!
//! `oak groups` lists the groups you're in. `oak group switch NAME` makes the
//! current checkout work as that group, so you see and change exactly what it
//! may: it makes a short-lived key whose grant is that group, and uses it for
//! this checkout only. `oak group switch --clear` goes back to your own access.
//!
//! `oak team` is the old name and still works.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::branch_target::bounded_body;
use crate::output;

/// 2: `oak team` became `oak group` (`team`/`teams` keys are `group`/`groups`,
/// groups carry `kind` and `repos`).
pub const SCHEMA_VERSION: u32 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a `switch` key works before it has to be renewed.
const SWITCH_KEY_SECONDS: i64 = 30 * 24 * 60 * 60;
/// The sidecar in a checkout's `.oak/` that records a switch.
const SWITCH_FILE: &str = "group-view.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoRole {
    pub repo: String,
    /// `reader` or `writer`.
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub slug: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub description: String,
    /// `custom`, `public` or `members`. Absent from servers before groups.
    #[serde(default = "custom_kind")]
    pub kind: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub maintainers: Vec<String>,
    /// The org's agents (service accounts) on this group. Absent from
    /// servers before agents could be put on groups.
    #[serde(default)]
    pub agents: Vec<String>,
    #[serde(default)]
    pub repos: Vec<RepoRole>,
}

/// One of the org's agents (service accounts), from
/// `GET /api/orgs/:org/service-accounts`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub disabled: bool,
    /// The groups it was put on, which are what it can reach.
    #[serde(default)]
    pub groups: Vec<String>,
}

fn custom_kind() -> String {
    "custom".to_string()
}

impl Group {
    fn is_builtin(&self) -> bool {
        self.kind != "custom"
    }
}

/// A group you're in, from `GET /api/user/groups`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Membership {
    pub org: String,
    pub slug: String,
    #[serde(default = "custom_kind")]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub maintainer: bool,
    /// This checkout is switched to this group (`oak group switch`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub active: bool,
}

/// What `oak group` does. Mirrors the clap subcommands in `main.rs`.
pub enum GroupAction {
    List,
    Show {
        slug: String,
    },
    Create {
        slug: String,
        parent: Option<String>,
        description: Option<String>,
    },
    Add {
        slug: String,
        username: String,
        maintainer: bool,
    },
    Remove {
        slug: String,
        username: String,
    },
    Invite {
        slug: String,
        username: String,
        maintainer: bool,
    },
    Grant {
        slug: String,
        repo: String,
        role: String,
    },
    Revoke {
        slug: String,
        repo: String,
    },
    AgentList,
    AgentAdd {
        slug: String,
        agent: String,
    },
    AgentRemove {
        slug: String,
        agent: String,
    },
}

pub struct OrgTarget {
    pub remote: String,
    pub org: String,
    pub token: Option<String>,
}

impl OrgTarget {
    pub fn new(remote: String, org: &str) -> Result<Self> {
        let org = validate_slug(org, "an org slug")?;
        let remote = remote.trim_end_matches('/').to_string();
        Ok(Self {
            token: super::credentials::effective_token(&remote),
            remote,
            org,
        })
    }

    fn url(&self, tail: &str) -> String {
        let base = format!(
            "{}/api/orgs/{}/groups",
            self.remote,
            urlencoding::encode(&self.org)
        );
        if tail.is_empty() {
            base
        } else {
            format!("{base}/{tail}")
        }
    }
}

fn send_with(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    let req = req.timeout(REQUEST_TIMEOUT);
    match token {
        Some(token) => req.header("authorization", format!("Bearer {token}")),
        None => req,
    }
}

fn validate_slug(value: &str, what: &str) -> Result<String> {
    let value = value
        .trim()
        .trim_start_matches("@group/")
        .trim_start_matches("@team/")
        .to_string();
    if value.is_empty()
        || value
            .chars()
            .any(|c| c == '/' || c.is_whitespace() || c.is_control())
    {
        return Err(OakError::InvalidArgument(format!(
            "expected a {what}, got {value:?}"
        )));
    }
    Ok(value)
}

/// `read`/`write` (or `reader`/`writer`) as the server spells it.
fn parse_role(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "read" | "reader" | "r" => Ok("reader".into()),
        "write" | "writer" | "w" => Ok("writer".into()),
        other => Err(OakError::InvalidArgument(format!(
            "--role must be read or write, got {other:?}"
        ))),
    }
}

fn role_word(role: &str) -> &'static str {
    if role == "writer" {
        "write"
    } else {
        "read"
    }
}

async fn call_with(
    remote: &str,
    org_hint: &str,
    token: Option<&str>,
    req: reqwest::RequestBuilder,
) -> Result<Value> {
    let response = send_with(req, token)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("groups request failed: {e}")))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(OakError::AuthRequired {
            remote: remote.to_string(),
            status: status.as_u16(),
        });
    }
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if !status.is_success() {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(match status {
            reqwest::StatusCode::NOT_FOUND => {
                OakError::GroupNotFound(format!("{org_hint} ({message})"))
            }
            reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::CONFLICT
            | reqwest::StatusCode::UNPROCESSABLE_ENTITY
            | reqwest::StatusCode::BAD_REQUEST => OakError::InvalidArgument(message),
            _ => OakError::Server(message),
        });
    }
    Ok(value)
}

/// A request to `/api/orgs/:org/groups/<tail>`. A server from before groups
/// answers that with a 404; the same request then goes to its old name,
/// `/teams/<tail>`, which takes the same bodies.
async fn call(
    target: &OrgTarget,
    method: reqwest::Method,
    tail: &str,
    body: Option<Value>,
) -> Result<Value> {
    let client = crate::http::api_client();
    let build = |url: String| {
        let req = client.request(method.clone(), url);
        match &body {
            Some(body) => req.json(body),
            None => req,
        }
    };
    let token = target.token.as_deref();
    match call_with(&target.remote, &target.org, token, build(target.url(tail))).await {
        Err(OakError::GroupNotFound(first)) => {
            let legacy = target.url(tail).replacen(
                &format!("/api/orgs/{}/groups", urlencoding::encode(&target.org)),
                &format!("/api/orgs/{}/teams", urlencoding::encode(&target.org)),
                1,
            );
            call_with(&target.remote, &target.org, token, build(legacy))
                .await
                .map_err(|e| match e {
                    OakError::GroupNotFound(_) => OakError::GroupNotFound(first),
                    e => e,
                })
        }
        other => other,
    }
}

/// Put an agent on a group, or take it off. There's no `/teams` fallback: a
/// server that predates groups predates agents on them too.
async fn agent_call(target: &OrgTarget, method: reqwest::Method, tail: &str) -> Result<Value> {
    let client = crate::http::api_client();
    match call_with(
        &target.remote,
        &target.org,
        target.token.as_deref(),
        client.request(method, target.url(tail)),
    )
    .await
    {
        // No such route: a 404 with no error message of ours, or the 405 an
        // older server's GET-only catch-all answers.
        Err(OakError::GroupNotFound(message)) if message.ends_with("(HTTP 404 Not Found)") => {
            Err(agents_unsupported())
        }
        Err(OakError::Server(message)) if message == "HTTP 405 Method Not Allowed" => {
            Err(agents_unsupported())
        }
        // The group exists but the agent doesn't.
        Err(OakError::GroupNotFound(message)) if message.contains("No agent named") => {
            Err(OakError::InvalidArgument(
                message
                    .split_once(" (")
                    .map(|(_, rest)| rest.trim_end_matches(')').to_string())
                    .unwrap_or(message),
            ))
        }
        other => other,
    }
}

fn agents_unsupported() -> OakError {
    OakError::Server("this server can't put agents on groups yet; it needs updating".into())
}

async fn list_agents(target: &OrgTarget) -> Result<Vec<Agent>> {
    let client = crate::http::api_client();
    let url = format!(
        "{}/api/orgs/{}/service-accounts",
        target.remote,
        urlencoding::encode(&target.org)
    );
    let value = call_with(
        &target.remote,
        &target.org,
        target.token.as_deref(),
        client.get(url),
    )
    .await?;
    serde_json::from_value(value).map_err(|_| OakError::Server("invalid agents response".into()))
}

fn parse_group(value: Value) -> Result<Group> {
    serde_json::from_value(value).map_err(|_| OakError::Server("invalid group response".into()))
}

async fn list_groups(target: &OrgTarget) -> Result<Vec<Group>> {
    let value = call(target, reqwest::Method::GET, "", None).await?;
    // A server from before groups lists them under `teams`.
    let list = if value["groups"].is_array() {
        &value["groups"]
    } else {
        &value["teams"]
    };
    serde_json::from_value(list.clone())
        .map_err(|_| OakError::Server("invalid groups response".into()))
}

/// The chain of parents above `slug`, nearest first.
fn ancestry(groups: &[Group], slug: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = groups.iter().find(|t| t.slug == slug);
    while let Some(group) = at {
        let Some(parent) = group.parent.as_deref() else {
            break;
        };
        if out.iter().any(|p| p == parent) || parent == slug {
            break;
        }
        out.push(parent.to_string());
        at = groups.iter().find(|t| t.slug == parent);
    }
    out
}

fn summary(group: &Group) -> String {
    let who = match group.kind.as_str() {
        "public" => "anyone".to_string(),
        "members" => format!("everyone in the org ({})", group.members.len()),
        _ => format!(
            "{} {}{}",
            group.members.len(),
            if group.members.len() == 1 {
                "person"
            } else {
                "people"
            },
            match group.agents.len() {
                0 => String::new(),
                1 => ", 1 agent".to_string(),
                n => format!(", {n} agents"),
            }
        ),
    };
    let repos = match group.repos.len() {
        0 => String::new(),
        1 => ", 1 repo".to_string(),
        n => format!(", {n} repos"),
    };
    format!("{who}{repos}")
}

fn print_tree(groups: &[Group], parent: Option<&str>, depth: usize, seen: &mut Vec<String>) {
    let mut children: Vec<&Group> = groups
        .iter()
        .filter(|t| t.parent.as_deref() == parent)
        .collect();
    // `public`, then `members`, then the rest by name (the server's order).
    children.sort_by_key(|g| {
        let rank = match g.kind.as_str() {
            "public" => 0,
            "members" => 1,
            _ => 2,
        };
        (rank, g.slug.clone())
    });
    for group in children {
        if seen.contains(&group.slug) {
            continue;
        }
        seen.push(group.slug.clone());
        output::print_line(&format!(
            "{}@group/{}  {}{}",
            "  ".repeat(depth),
            group.slug,
            summary(group),
            if group.description.is_empty() || group.is_builtin() {
                String::new()
            } else {
                format!("  {}", group.description)
            }
        ));
        print_tree(groups, Some(&group.slug), depth + 1, seen);
    }
}

#[derive(Serialize)]
struct GroupsJson<'a> {
    schema_version: u32,
    org: &'a str,
    groups: &'a [Group],
}

#[derive(Serialize)]
struct GroupJson<'a> {
    schema_version: u32,
    org: &'a str,
    group: &'a Group,
    /// Its parents, nearest first: membership counts for each of them.
    ancestors: Vec<String>,
}

pub async fn run(remote: String, org: &str, action: GroupAction, json_output: bool) -> Result<()> {
    let target = OrgTarget::new(remote, org)?;
    match action {
        GroupAction::List => {
            let groups = list_groups(&target).await?;
            if json_output {
                return output::print_json(&GroupsJson {
                    schema_version: SCHEMA_VERSION,
                    org: &target.org,
                    groups: &groups,
                });
            }
            let mut seen = Vec::new();
            print_tree(&groups, None, 0, &mut seen);
            // A group whose parent isn't listed still shows.
            for group in &groups {
                if !seen.contains(&group.slug) {
                    print_tree(&groups, group.parent.as_deref(), 0, &mut seen);
                }
            }
            Ok(())
        }
        GroupAction::Show { slug } => {
            let slug = validate_slug(&slug, "group name")?;
            let groups = list_groups(&target).await?;
            let group = groups
                .iter()
                .find(|t| t.slug == slug)
                .ok_or_else(|| OakError::GroupNotFound(slug.clone()))?;
            print_group(&target.org, group, ancestry(&groups, &slug), json_output)
        }
        GroupAction::Create {
            slug,
            parent,
            description,
        } => {
            let slug = validate_slug(&slug, "group name")?;
            let mut body = json!({ "slug": slug });
            if let Some(parent) = parent {
                body["parent"] = json!(validate_slug(&parent, "group name")?);
            }
            if let Some(description) = description {
                body["description"] = json!(description);
            }
            let group = parse_group(call(&target, reqwest::Method::POST, "", Some(body)).await?)?;
            if !json_output {
                output::success(&format!("Created @group/{}", group.slug));
            }
            print_group(&target.org, &group, Vec::new(), json_output)
        }
        GroupAction::Add {
            slug,
            username,
            maintainer,
        } => {
            let slug = validate_slug(&slug, "group name")?;
            let username = validate_slug(username.trim_start_matches('@'), "username")?;
            let tail = format!(
                "{}/members/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&username)
            );
            let group = parse_group(
                call(
                    &target,
                    reqwest::Method::PUT,
                    &tail,
                    Some(json!({ "maintainer": maintainer })),
                )
                .await?,
            )?;
            if !json_output {
                output::success(&format!(
                    "@{username} is {} of @group/{slug}",
                    if maintainer {
                        "a maintainer"
                    } else {
                        "a member"
                    }
                ));
            }
            print_group(&target.org, &group, Vec::new(), json_output)
        }
        GroupAction::Remove { slug, username } => {
            let slug = validate_slug(&slug, "group name")?;
            let username = validate_slug(username.trim_start_matches('@'), "username")?;
            let tail = format!(
                "{}/members/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&username)
            );
            let group = parse_group(call(&target, reqwest::Method::DELETE, &tail, None).await?)?;
            if !json_output {
                output::success(&format!("Removed @{username} from @group/{slug}"));
            }
            print_group(&target.org, &group, Vec::new(), json_output)
        }
        GroupAction::Invite {
            slug,
            username,
            maintainer,
        } => {
            let slug = validate_slug(&slug, "group name")?;
            let username = validate_slug(username.trim_start_matches('@'), "username")?;
            let tail = format!("{}/invite", urlencoding::encode(&slug));
            let value = call(
                &target,
                reqwest::Method::POST,
                &tail,
                Some(json!({ "username": username, "maintainer": maintainer })),
            )
            .await?;
            let joined_org = value["joined_org"].as_bool().unwrap_or(false);
            if json_output {
                return output::print_json(&json!({
                    "schema_version": SCHEMA_VERSION,
                    "org": target.org,
                    "group": value["group"],
                    "username": username,
                    "joined_org": joined_org,
                }));
            }
            if joined_org {
                output::success(&format!(
                    "@{username} joined {} and @group/{slug}",
                    target.org
                ));
            } else {
                output::success(&format!("@{username} is in @group/{slug}"));
            }
            Ok(())
        }
        GroupAction::Grant { slug, repo, role } => {
            let slug = validate_slug(&slug, "group name")?;
            let repo = repo_name(&target.org, &repo)?;
            let role = parse_role(&role)?;
            let tail = format!(
                "{}/repos/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&repo)
            );
            let value = call(
                &target,
                reqwest::Method::PUT,
                &tail,
                Some(json!({ "role": role })),
            )
            .await?;
            if json_output {
                return output::print_json(&access_json(&target.org, &value));
            }
            if slug == "public" {
                output::success(&format!("{}/{repo} is public", target.org));
            } else {
                output::success(&format!(
                    "@group/{slug} can {} {}/{repo}",
                    role_word(&role),
                    target.org
                ));
            }
            Ok(())
        }
        GroupAction::Revoke { slug, repo } => {
            let slug = validate_slug(&slug, "group name")?;
            let repo = repo_name(&target.org, &repo)?;
            let tail = format!(
                "{}/repos/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&repo)
            );
            let value = call(&target, reqwest::Method::DELETE, &tail, None).await?;
            if json_output {
                return output::print_json(&access_json(&target.org, &value));
            }
            if slug == "public" {
                output::success(&format!("{}/{repo} is private", target.org));
            } else {
                output::success(&format!(
                    "@group/{slug} no longer has access to {}/{repo}",
                    target.org
                ));
            }
            Ok(())
        }
        GroupAction::AgentList => {
            let agents = list_agents(&target).await?;
            if json_output {
                return output::print_json(&AgentsJson {
                    schema_version: SCHEMA_VERSION,
                    org: &target.org,
                    agents: &agents,
                });
            }
            if agents.is_empty() {
                output::print_line(&format!(
                    "{} has no agents. Org owners and admins create them under the org's \
                     Settings → Agents.",
                    target.org
                ));
                return Ok(());
            }
            let width = agents.iter().map(|a| a.name.len()).max().unwrap_or(0);
            for agent in &agents {
                output::print_line(&format!("{:width$}  {}", agent.name, agent_line(agent)));
            }
            Ok(())
        }
        GroupAction::AgentAdd { slug, agent } => {
            let slug = validate_slug(&slug, "group name")?;
            let agent = validate_slug(&agent, "agent name")?;
            let tail = format!(
                "{}/agents/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&agent)
            );
            let group = parse_group(agent_call(&target, reqwest::Method::PUT, &tail).await?)?;
            if !json_output {
                output::success(&format!(
                    "{agent} is on @group/{slug}: its keys reach what the group reaches"
                ));
            }
            print_group(&target.org, &group, Vec::new(), json_output)
        }
        GroupAction::AgentRemove { slug, agent } => {
            let slug = validate_slug(&slug, "group name")?;
            let agent = validate_slug(&agent, "agent name")?;
            let tail = format!(
                "{}/agents/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&agent)
            );
            let group = parse_group(agent_call(&target, reqwest::Method::DELETE, &tail).await?)?;
            if !json_output {
                output::success(&format!("Took {agent} off @group/{slug}"));
            }
            print_group(&target.org, &group, Vec::new(), json_output)
        }
    }
}

#[derive(Serialize)]
struct AgentsJson<'a> {
    schema_version: u32,
    org: &'a str,
    agents: &'a [Agent],
}

/// What `oak group agent list` says after an agent's name.
fn agent_line(agent: &Agent) -> String {
    let groups = if agent.groups.is_empty() {
        "no groups (public repos only)".to_string()
    } else {
        agent
            .groups
            .iter()
            .map(|g| format!("@group/{g}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if agent.disabled {
        format!("{groups}  disabled")
    } else {
        groups
    }
}

/// `REPO` or `ORG/REPO` (which must be `org`'s) to the repo's name.
fn repo_name(org: &str, raw: &str) -> Result<String> {
    let raw = raw.trim();
    let name = match raw.split_once('/') {
        Some((owner, name)) if owner.eq_ignore_ascii_case(org) => name,
        Some((owner, _)) => {
            return Err(OakError::InvalidArgument(format!(
                "{raw} isn't in {org}; groups only get repos of their own org (not {owner})"
            )))
        }
        None => raw,
    };
    validate_slug(name, "repository name")
}

fn access_json(org: &str, value: &Value) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "org": org,
        "repo": value["repo"],
        "is_public": value["is_public"],
        "grants": value["grants"],
    })
}

fn print_group(org: &str, group: &Group, ancestors: Vec<String>, json_output: bool) -> Result<()> {
    if json_output {
        return output::print_json(&GroupJson {
            schema_version: SCHEMA_VERSION,
            org,
            group,
            ancestors,
        });
    }
    output::print_line(&format!("@group/{}", group.slug));
    if !group.description.is_empty() {
        output::print_line(&format!("  {}", group.description));
    }
    if let Some(parent) = &group.parent {
        output::print_line(&format!("  sub-group of @group/{parent}"));
    }
    match group.kind.as_str() {
        "public" => output::print_line("  built in: anyone, including people who aren't signed in"),
        "members" => {
            output::print_line(&format!(
                "  built in: everyone in {org} (`oak group add|remove members USER` adds to or \
                 removes from the org)"
            ));
            output::print_line(&format!(
                "  owners and admins: {}",
                group.maintainers.join(", ")
            ));
            output::print_line(&format!("  members: {}", group.members.join(", ")));
        }
        _ => {
            output::print_line(&format!(
                "  maintainers: {}",
                if group.maintainers.is_empty() {
                    "(org owners and admins)".to_string()
                } else {
                    group.maintainers.join(", ")
                }
            ));
            output::print_line(&format!(
                "  members: {}",
                if group.members.is_empty() {
                    "(none)".to_string()
                } else {
                    group.members.join(", ")
                }
            ));
            if !group.agents.is_empty() {
                output::print_line(&format!("  agents: {}", group.agents.join(", ")));
            }
        }
    }
    if group.repos.is_empty() {
        output::print_line("  repos: (none)");
    } else {
        output::print_line("  repos:");
        for r in &group.repos {
            output::print_line(&format!("    {}/{}  {}", org, r.repo, role_word(&r.role)));
        }
    }
    Ok(())
}

// -- oak groups ---------------------------------------------------------------

#[derive(Serialize)]
struct MineJson<'a> {
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    org: Option<&'a str>,
    groups: &'a [Membership],
    /// The group this checkout is switched to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<&'a SwitchState>,
}

/// `oak groups`: the groups you're in, across your orgs, or in `org`. The one
/// a checkout at `cwd` is switched to is marked.
pub async fn mine(remote: String, org: Option<&str>, cwd: &Path, json_output: bool) -> Result<()> {
    let remote = remote.trim_end_matches('/').to_string();
    // Always ask as yourself, not as a group this checkout switched to.
    let token = super::credentials::get_token_for_server(&remote)
        .or_else(|| super::credentials::effective_token(&remote));
    let client = crate::http::api_client();
    let value = call_with(
        &remote,
        "your groups",
        token.as_deref(),
        client.get(format!("{remote}/api/user/groups")),
    )
    .await?;
    let mut groups: Vec<Membership> = serde_json::from_value(value["groups"].clone())
        .map_err(|_| OakError::Server("invalid groups response".into()))?;
    if let Some(org) = org {
        groups.retain(|g| g.org.eq_ignore_ascii_case(org));
    }
    let active = switch_state(cwd).ok().flatten();
    for g in &mut groups {
        g.active = active
            .as_ref()
            .is_some_and(|a| a.org.eq_ignore_ascii_case(&g.org) && a.group == g.slug);
    }
    if json_output {
        return output::print_json(&MineJson {
            schema_version: SCHEMA_VERSION,
            org,
            groups: &groups,
            active: active.as_ref(),
        });
    }
    if groups.is_empty() {
        output::print_line(match org {
            Some(_) => "You aren't in any groups there.",
            None => "You aren't in any groups.",
        });
        return Ok(());
    }
    let mut last_org = String::new();
    for g in &groups {
        if org.is_none() && g.org != last_org {
            output::print_line(&g.org);
            last_org = g.org.clone();
        }
        let indent = if org.is_none() { "  " } else { "" };
        let mut notes = Vec::new();
        if g.kind == "members" {
            notes.push("everyone in the org".to_string());
        }
        if g.maintainer {
            notes.push("maintainer".to_string());
        }
        output::print_line(&format!(
            "{indent}{} @group/{}{}",
            if g.active { "*" } else { " " },
            g.slug,
            if notes.is_empty() {
                String::new()
            } else {
                format!("  ({})", notes.join(", "))
            }
        ));
    }
    if let Some(a) = &active {
        output::print_line(&format!(
            "\nThis checkout works as @group/{} ({}). `oak group switch --clear` goes back to your own access.",
            a.group, a.org
        ));
    }
    Ok(())
}

// -- oak group switch ---------------------------------------------------------

/// What a switch left in `.oak/group-view.json`, so `--clear` can undo it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwitchState {
    pub org: String,
    pub group: String,
    pub remote: String,
    /// The key made for the switch, to revoke on `--clear`.
    pub key_id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The checkout's own key before the switch, put back on `--clear`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_key: Option<String>,
}

fn switch_path(oak_dir: &Path) -> PathBuf {
    oak_dir.join(SWITCH_FILE)
}

fn switch_state(cwd: &Path) -> Result<Option<SwitchState>> {
    let context = crate::resolve::resolve(cwd)?;
    read_switch(&context.oak_dir)
}

fn read_switch(oak_dir: &Path) -> Result<Option<SwitchState>> {
    match std::fs::read(switch_path(oak_dir)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OakError::Io(e)),
    }
}

#[derive(Serialize)]
struct SwitchJson<'a> {
    schema_version: u32,
    /// `switched`, `cleared`, `unchanged` or `current`.
    outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<&'a SwitchState>,
}

/// `oak group switch [NAME] [--clear]` in the checkout at `cwd`.
pub async fn switch(
    cwd: &Path,
    slug: Option<String>,
    clear: bool,
    json_output: bool,
) -> Result<()> {
    use oak_core::{MetadataKey, Repository, SqliteRepository};

    let context = crate::resolve::resolve(cwd)?;
    let oak_dir = context.oak_dir.clone();
    let current = read_switch(&oak_dir)?;
    let workspace = super::branch::RemoteWorkspace::open(cwd)?;
    let remote = workspace
        .remote
        .remote_url
        .trim_end_matches('/')
        .to_string();
    let org = workspace.remote.owner.clone();
    let repo_name = workspace.remote.repo_name.clone();
    let repo: &SqliteRepository = &workspace.repo;
    // Everything here is done as you, never as the group being left.
    let account = super::credentials::get_token_for_server(&remote).ok_or_else(|| {
        OakError::AuthRequired {
            remote: remote.clone(),
            status: 401,
        }
    })?;
    let client = crate::http::api_client();

    let restore = |state: &SwitchState| -> Result<()> {
        match &state.previous_key {
            Some(key) => super::credentials::store_repository_key(repo, &remote, key),
            None => {
                repo.delete_metadata(MetadataKey::ApiKey)?;
                repo.delete_metadata(MetadataKey::ApiKeyOrigin)
            }
        }
    };
    let revoke = |key_id: i64| {
        let client = client.clone();
        let url = format!("{remote}/api/api-keys/{key_id}");
        let account = account.clone();
        async move {
            // Best effort: an expired or deleted key is already gone.
            let _ = send_with(client.delete(url), Some(&account)).send().await;
        }
    };

    if clear || slug.as_deref() == Some("-") {
        let Some(state) = current else {
            if json_output {
                return output::print_json(&SwitchJson {
                    schema_version: SCHEMA_VERSION,
                    outcome: "unchanged",
                    active: None,
                });
            }
            output::print_line("This checkout already works with your own access.");
            return Ok(());
        };
        restore(&state)?;
        revoke(state.key_id).await;
        let _ = std::fs::remove_file(switch_path(&oak_dir));
        if json_output {
            return output::print_json(&SwitchJson {
                schema_version: SCHEMA_VERSION,
                outcome: "cleared",
                active: None,
            });
        }
        output::success(&format!(
            "Back to your own access (was @group/{})",
            state.group
        ));
        return Ok(());
    }

    let Some(slug) = slug else {
        if json_output {
            return output::print_json(&SwitchJson {
                schema_version: SCHEMA_VERSION,
                outcome: "current",
                active: current.as_ref(),
            });
        }
        match &current {
            Some(state) => output::print_line(&format!(
                "This checkout works as @group/{} ({}).",
                state.group, state.org
            )),
            None => output::print_line("This checkout works with your own access."),
        }
        return Ok(());
    };
    let slug = validate_slug(&slug, "group name")?;
    if current.as_ref().is_some_and(|c| c.group == slug) {
        if json_output {
            return output::print_json(&SwitchJson {
                schema_version: SCHEMA_VERSION,
                outcome: "unchanged",
                active: current.as_ref(),
            });
        }
        output::print_line(&format!("This checkout already works as @group/{slug}."));
        return Ok(());
    }

    // A key for this checkout whose grant is the group. The server checks
    // that you're in it (or run the org) and keeps checking on every request.
    let host = remote
        .split("://")
        .nth(1)
        .unwrap_or(&remote)
        .trim_end_matches('/');
    let name = format!("oak group switch {slug} ({org}/{repo_name} on {host})");
    let created = call_with(
        &remote,
        &org,
        Some(&account),
        client.post(format!("{remote}/api/api-keys")).json(&json!({
            "name": name,
            "org": org,
            "groups": [slug],
            "expires_in": SWITCH_KEY_SECONDS,
        })),
    )
    .await
    .map_err(|e| match e {
        OakError::InvalidArgument(m) => {
            OakError::InvalidArgument(format!("can't switch to @group/{slug}: {m}"))
        }
        e => e,
    })?;
    let key = created["key"]
        .as_str()
        .ok_or_else(|| OakError::Server("invalid key response".into()))?
        .to_string();
    let key_id = created["id"]
        .as_i64()
        .ok_or_else(|| OakError::Server("invalid key response".into()))?;

    // A checkout of the whole repo can't work as a group that can't read all
    // of it: pulls would be refused. Check before changing anything.
    let target = super::access::RepoTarget::for_spec_with_token(&remote, &org, &repo_name, &key);
    let checkout_is_view = repo
        .get_metadata(MetadataKey::ViewSpec)
        .ok()
        .flatten()
        .is_some();
    if let Some(hidden) = super::access::hidden_directories(&target).await? {
        if hidden > 0 && !checkout_is_view {
            revoke(key_id).await;
            return Err(OakError::InvalidArgument(format!(
                "@group/{slug} can't read {hidden} folder(s) of {org}/{repo_name}, and this \
                 checkout holds all of it. Switch in a checkout of the folders the group can \
                 read: `oak clone {org}/{repo_name} --path <folder>`, then `oak group switch \
                 {slug}` there."
            )));
        }
    }

    // Keep the checkout's own key (not a previous switch's) to put back.
    let previous_key = match &current {
        Some(state) => {
            revoke(state.key_id).await;
            state.previous_key.clone()
        }
        None => super::credentials::repository_credential(repo).and_then(|c| c.token_for(&remote)),
    };
    super::credentials::store_repository_key(repo, &remote, &key)?;
    let state = SwitchState {
        org: org.clone(),
        group: slug.clone(),
        remote: remote.clone(),
        key_id,
        expires_at: created["expires_at"].as_str().map(str::to_string),
        previous_key,
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|e| OakError::Server(format!("serializing switch state: {e}")))?;
    write_private(&switch_path(&oak_dir), &bytes)?;
    if json_output {
        return output::print_json(&SwitchJson {
            schema_version: SCHEMA_VERSION,
            outcome: "switched",
            active: Some(&state),
        });
    }
    output::success(&format!(
        "This checkout now works as @group/{slug}: you see and change what the group can"
    ));
    output::print_line(
        "Merging and approving still take your own access. `oak group switch --clear` goes back.",
    );
    Ok(())
}

/// Write a file only its owner can read (it holds a key).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).map_err(OakError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(OakError::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(slug: &str, parent: Option<&str>) -> Group {
        Group {
            slug: slug.to_string(),
            parent: parent.map(str::to_string),
            description: String::new(),
            kind: "custom".into(),
            members: Vec::new(),
            maintainers: Vec::new(),
            agents: Vec::new(),
            repos: Vec::new(),
        }
    }

    #[test]
    fn an_agent_line_names_its_groups_or_says_it_has_none() {
        let agent = |groups: &[&str], disabled| Agent {
            name: "bot".into(),
            display_name: String::new(),
            disabled,
            groups: groups.iter().map(|g| g.to_string()).collect(),
        };
        assert_eq!(
            agent_line(&agent(&["web", "ops"], false)),
            "@group/web, @group/ops"
        );
        assert_eq!(
            agent_line(&agent(&[], false)),
            "no groups (public repos only)"
        );
        assert_eq!(agent_line(&agent(&["web"], true)), "@group/web  disabled");
        // A server from before agents were on groups sends no `groups`.
        let old: Agent = serde_json::from_value(json!({"name": "bot", "disabled": false})).unwrap();
        assert!(old.groups.is_empty());
    }

    #[test]
    fn ancestry_walks_parents_and_stops_on_a_cycle() {
        let groups = [
            group("eng", None),
            group("web", Some("eng")),
            group("frontend", Some("web")),
            group("a", Some("b")),
            group("b", Some("a")),
        ];
        assert_eq!(ancestry(&groups, "frontend"), ["web", "eng"]);
        assert!(ancestry(&groups, "eng").is_empty());
        assert_eq!(ancestry(&groups, "a"), ["b"]);
    }

    #[test]
    fn slugs_drop_the_group_and_team_prefixes_and_refuse_paths() {
        assert_eq!(validate_slug("@group/web", "group name").unwrap(), "web");
        assert_eq!(validate_slug("@team/web", "group name").unwrap(), "web");
        assert!(validate_slug("eng/web", "group name").is_err());
        assert!(validate_slug(" ", "group name").is_err());
    }

    #[test]
    fn roles_and_repo_names() {
        assert_eq!(parse_role("Write").unwrap(), "writer");
        assert_eq!(parse_role("read").unwrap(), "reader");
        assert!(parse_role("admin").is_err());
        assert_eq!(repo_name("acme", "acme/web").unwrap(), "web");
        assert_eq!(repo_name("acme", "web").unwrap(), "web");
        assert!(repo_name("acme", "other/web").is_err());
    }

    #[test]
    fn old_servers_without_kinds_read_as_custom() {
        let g: Group = serde_json::from_value(json!({"slug": "web"})).unwrap();
        assert_eq!(g.kind, "custom");
        assert!(g.repos.is_empty());
    }
}
