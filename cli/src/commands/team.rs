//! `oak team`: an org's teams (`/api/orgs/:org/teams`).
//!
//! Teams are lists of people that ownership boundaries name (`@team/web`).
//! They can be nested: a member of a sub-team counts as a member of every
//! team above it, and a maintainer of a team runs it and every team below it.
//! Org owners and admins run them all.

use std::time::Duration;

use oak_core::{OakError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::findings::bounded_body;
use crate::output;

pub const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Team {
    pub slug: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub maintainers: Vec<String>,
}

/// What `oak team` does. Mirrors the clap subcommands in `main.rs`.
pub enum TeamAction {
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
}

struct OrgTarget {
    remote: String,
    org: String,
    token: Option<String>,
}

impl OrgTarget {
    fn url(&self, tail: &str) -> String {
        let base = format!(
            "{}/api/orgs/{}/teams",
            self.remote,
            urlencoding::encode(&self.org)
        );
        if tail.is_empty() {
            base
        } else {
            format!("{base}/{tail}")
        }
    }

    fn send(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req.timeout(REQUEST_TIMEOUT);
        match &self.token {
            Some(token) => req.header("authorization", format!("Bearer {token}")),
            None => req,
        }
    }
}

fn validate_slug(value: &str, what: &str) -> Result<String> {
    let value = value.trim().trim_start_matches("@team/").to_string();
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

async fn call(target: &OrgTarget, req: reqwest::RequestBuilder) -> Result<Value> {
    let response = target
        .send(req)
        .send()
        .await
        .map_err(|e| OakError::Http(format!("teams request failed: {e}")))?;
    let status = response.status();
    let body = bounded_body(response).await?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(OakError::AuthRequired {
            remote: target.remote.clone(),
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
            reqwest::StatusCode::NOT_FOUND => OakError::TeamNotFound(format!(
                "{} ({message}; teams may not be on for this org)",
                target.org
            )),
            reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::CONFLICT
            | reqwest::StatusCode::UNPROCESSABLE_ENTITY
            | reqwest::StatusCode::BAD_REQUEST => OakError::InvalidArgument(message),
            _ => OakError::Server(message),
        });
    }
    Ok(value)
}

fn parse_team(value: Value) -> Result<Team> {
    serde_json::from_value(value).map_err(|_| OakError::Server("invalid team response".into()))
}

/// The chain of parents above `slug`, nearest first.
fn ancestry(teams: &[Team], slug: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = teams.iter().find(|t| t.slug == slug);
    while let Some(team) = at {
        let Some(parent) = team.parent.as_deref() else {
            break;
        };
        if out.iter().any(|p| p == parent) || parent == slug {
            break;
        }
        out.push(parent.to_string());
        at = teams.iter().find(|t| t.slug == parent);
    }
    out
}

fn print_tree(teams: &[Team], parent: Option<&str>, depth: usize, seen: &mut Vec<String>) {
    let mut children: Vec<&Team> = teams
        .iter()
        .filter(|t| t.parent.as_deref() == parent)
        .collect();
    children.sort_by(|a, b| a.slug.cmp(&b.slug));
    for team in children {
        if seen.contains(&team.slug) {
            continue;
        }
        seen.push(team.slug.clone());
        output::print_line(&format!(
            "{}@team/{}  {} member(s){}",
            "  ".repeat(depth),
            team.slug,
            team.members.len(),
            if team.description.is_empty() {
                String::new()
            } else {
                format!("  {}", team.description)
            }
        ));
        print_tree(teams, Some(&team.slug), depth + 1, seen);
    }
}

#[derive(Serialize)]
struct TeamsJson<'a> {
    schema_version: u32,
    org: &'a str,
    teams: &'a [Team],
}

#[derive(Serialize)]
struct TeamJson<'a> {
    schema_version: u32,
    org: &'a str,
    team: &'a Team,
    /// Its parents, nearest first: membership counts for each of them.
    ancestors: Vec<String>,
}

pub async fn run(remote: String, org: &str, action: TeamAction, json_output: bool) -> Result<()> {
    let org = validate_slug(org, "an org slug")?;
    let remote = remote.trim_end_matches('/').to_string();
    let target = OrgTarget {
        token: super::credentials::effective_token(&remote),
        remote,
        org,
    };
    let client = crate::http::api_client();
    match action {
        TeamAction::List => {
            let value = call(&target, client.get(target.url(""))).await?;
            let teams: Vec<Team> = serde_json::from_value(value["teams"].clone())
                .map_err(|_| OakError::Server("invalid teams response".into()))?;
            if json_output {
                return output::print_json(&TeamsJson {
                    schema_version: SCHEMA_VERSION,
                    org: &target.org,
                    teams: &teams,
                });
            }
            if teams.is_empty() {
                output::print_line(&format!("{} has no teams.", target.org));
                return Ok(());
            }
            let mut seen = Vec::new();
            print_tree(&teams, None, 0, &mut seen);
            // A team whose parent isn't listed still shows.
            for team in &teams {
                if !seen.contains(&team.slug) {
                    print_tree(&teams, team.parent.as_deref(), 0, &mut seen);
                }
            }
            Ok(())
        }
        TeamAction::Show { slug } => {
            let slug = validate_slug(&slug, "team slug")?;
            let value = call(&target, client.get(target.url(""))).await?;
            let teams: Vec<Team> = serde_json::from_value(value["teams"].clone())
                .map_err(|_| OakError::Server("invalid teams response".into()))?;
            let team = teams
                .iter()
                .find(|t| t.slug == slug)
                .ok_or_else(|| OakError::TeamNotFound(slug.clone()))?;
            print_team(&target.org, team, ancestry(&teams, &slug), json_output)
        }
        TeamAction::Create {
            slug,
            parent,
            description,
        } => {
            let slug = validate_slug(&slug, "team slug")?;
            let mut body = json!({ "slug": slug });
            if let Some(parent) = parent {
                body["parent"] = json!(validate_slug(&parent, "team slug")?);
            }
            if let Some(description) = description {
                body["description"] = json!(description);
            }
            let team = parse_team(call(&target, client.post(target.url("")).json(&body)).await?)?;
            if !json_output {
                output::success(&format!("Created @team/{}", team.slug));
            }
            print_team(&target.org, &team, Vec::new(), json_output)
        }
        TeamAction::Add {
            slug,
            username,
            maintainer,
        } => {
            let slug = validate_slug(&slug, "team slug")?;
            let username = validate_slug(username.trim_start_matches('@'), "username")?;
            let url = target.url(&format!(
                "{}/members/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&username)
            ));
            let team = parse_team(
                call(
                    &target,
                    client.put(url).json(&json!({ "maintainer": maintainer })),
                )
                .await?,
            )?;
            if !json_output {
                output::success(&format!(
                    "@{username} is {} of @team/{slug}",
                    if maintainer {
                        "a maintainer"
                    } else {
                        "a member"
                    }
                ));
            }
            print_team(&target.org, &team, Vec::new(), json_output)
        }
        TeamAction::Remove { slug, username } => {
            let slug = validate_slug(&slug, "team slug")?;
            let username = validate_slug(username.trim_start_matches('@'), "username")?;
            let url = target.url(&format!(
                "{}/members/{}",
                urlencoding::encode(&slug),
                urlencoding::encode(&username)
            ));
            let team = parse_team(call(&target, client.delete(url)).await?)?;
            if !json_output {
                output::success(&format!("Removed @{username} from @team/{slug}"));
            }
            print_team(&target.org, &team, Vec::new(), json_output)
        }
    }
}

fn print_team(org: &str, team: &Team, ancestors: Vec<String>, json_output: bool) -> Result<()> {
    if json_output {
        return output::print_json(&TeamJson {
            schema_version: SCHEMA_VERSION,
            org,
            team,
            ancestors,
        });
    }
    output::print_line(&format!("@team/{}", team.slug));
    if !team.description.is_empty() {
        output::print_line(&format!("  {}", team.description));
    }
    if let Some(parent) = &team.parent {
        output::print_line(&format!("  sub-team of @team/{parent}"));
    }
    output::print_line(&format!(
        "  maintainers: {}",
        if team.maintainers.is_empty() {
            "(org owners and admins)".to_string()
        } else {
            team.maintainers.join(", ")
        }
    ));
    output::print_line(&format!(
        "  members: {}",
        if team.members.is_empty() {
            "(none)".to_string()
        } else {
            team.members.join(", ")
        }
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(slug: &str, parent: Option<&str>) -> Team {
        Team {
            slug: slug.to_string(),
            parent: parent.map(str::to_string),
            description: String::new(),
            members: Vec::new(),
            maintainers: Vec::new(),
        }
    }

    #[test]
    fn ancestry_walks_parents_and_stops_on_a_cycle() {
        let teams = [
            team("eng", None),
            team("web", Some("eng")),
            team("frontend", Some("web")),
            team("a", Some("b")),
            team("b", Some("a")),
        ];
        assert_eq!(ancestry(&teams, "frontend"), ["web", "eng"]);
        assert!(ancestry(&teams, "eng").is_empty());
        assert_eq!(ancestry(&teams, "a"), ["b"]);
    }

    #[test]
    fn slugs_drop_the_team_prefix_and_refuse_paths() {
        assert_eq!(validate_slug("@team/web", "team slug").unwrap(), "web");
        assert!(validate_slug("eng/web", "team slug").is_err());
        assert!(validate_slug(" ", "team slug").is_err());
    }
}
