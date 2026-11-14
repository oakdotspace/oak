//! `oak access`, `oak owners` and `oak team` against a server, with no
//! checkout (`--repo`, `--org`), plus how `oak mount --path` decides whether a
//! server serves partial mounts.

use std::process::{Command, Output};

use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn boundary(id: i64, dir: &str, team: &str, approval: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "path": dir,
        "owner_team": team,
        "approval": approval,
        "self_approve": false,
        "readers": null,
        "writers": null,
        "approvers": [format!("@group/{team}")],
        "can_edit": false,
    })
}

async fn server() -> MockServer {
    let server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/api/acme/mono/boundaries"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "boundaries": [
                boundary(1, "", "platform", "advisory"),
                boundary(2, "apps/web", "web", "required"),
                boundary(3, "apps/web/admin", "admin", "off"),
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/acme/plain/boundaries"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({"error": "Not found", "code": "not_found"})),
        )
        .mount(&server)
        .await;
    let teams = serde_json::json!({"teams": [
        {"slug": "eng", "parent": null, "description": "Engineering",
         "members": ["alice"], "maintainers": ["alice"]},
        {"slug": "web", "parent": "eng", "description": "",
         "members": ["bob", "carol"], "maintainers": ["bob"]},
        {"slug": "frontend", "parent": "web", "description": "",
         "members": ["carol"], "maintainers": []},
    ]});
    Mock::given(method("GET"))
        .and(path("/api/orgs/acme/teams"))
        .respond_with(ResponseTemplate::new(200).set_body_json(teams))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/orgs/acme/teams"))
        .and(body_partial_json(
            serde_json::json!({"slug": "design", "parent": "eng"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "slug": "design", "parent": "eng", "description": "",
            "members": [], "maintainers": [],
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/orgs/acme/teams/web/members/dave"))
        .and(body_partial_json(serde_json::json!({"maintainer": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "slug": "web", "parent": "eng", "description": "",
            "members": ["bob", "carol", "dave"], "maintainers": ["bob", "dave"],
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/orgs/acme/teams/web/members/zed"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "error": "zed isn't a member of web", "code": "invalid",
        })))
        .mount(&server)
        .await;
    server
}

async fn oak(args: Vec<String>) -> Output {
    let home = tempfile::TempDir::new().unwrap();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_oak"))
            .args(&args)
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("OAK_NO_UPDATE_CHECK", "1")
            .env_remove("OAK_REMOTE")
            .env_remove("OAK_API_KEY")
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "not JSON ({e}): stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[tokio::test]
async fn access_lists_the_boundaries_and_owners_picks_the_deepest() {
    let server = server().await;
    let uri = server.uri();

    let out = oak(args(&[
        "access",
        "--repo",
        "acme/mono",
        "--remote",
        &uri,
        "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    let body = json_of(&out);
    assert_eq!(body["repo"], "acme/mono");
    assert_eq!(body["boundaries"].as_array().unwrap().len(), 3);
    assert_eq!(body["boundaries"][1]["owner_team"], "web");

    let out = oak(args(&[
        "owners",
        "apps/web/src/main.ts",
        "--repo",
        "acme/mono",
        "--remote",
        &uri,
        "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    let body = json_of(&out);
    assert_eq!(body["path"], "apps/web/src/main.ts");
    assert_eq!(body["boundary"]["path"], "apps/web");
    assert_eq!(body["boundary"]["approval"], "required");

    let out = oak(args(&[
        "owners",
        "/apps/web/admin/",
        "--repo",
        "acme/mono",
        "--remote",
        &uri,
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("@group/admin"), "{text}");

    // A repo without teams: a clear error, not an empty list.
    let out = oak(args(&[
        "access",
        "--repo",
        "acme/plain",
        "--remote",
        &uri,
        "--json",
    ]))
    .await;
    assert!(!out.status.success());
    let err =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("path rules aren't on"), "{err}");
}

/// `oak team` is `oak group`'s old name, and against a server from before
/// groups (only `/teams`) every subcommand falls back to the old routes.
#[tokio::test]
async fn group_commands_work_against_a_server_from_before_groups() {
    let server = server().await;
    let uri = server.uri();

    let out = oak(args(&[
        "team", "list", "--org", "acme", "--remote", &uri, "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    let body = json_of(&out);
    assert_eq!(body["schema_version"], 2);
    assert_eq!(body["groups"].as_array().unwrap().len(), 3);

    let out = oak(args(&["group", "list", "--org", "acme", "--remote", &uri])).await;
    let text = String::from_utf8_lossy(&out.stdout);
    let eng = text.find("@group/eng").unwrap();
    let web = text.find("  @group/web").unwrap();
    let frontend = text.find("    @group/frontend").unwrap();
    assert!(eng < web && web < frontend, "a tree: {text}");

    let out = oak(args(&[
        "group", "show", "frontend", "--org", "acme", "--remote", &uri, "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    let body = json_of(&out);
    assert_eq!(body["group"]["slug"], "frontend");
    assert_eq!(body["group"]["kind"], "custom");
    assert_eq!(body["ancestors"], serde_json::json!(["web", "eng"]));

    let out = oak(args(&[
        "group", "create", "design", "--parent", "eng", "--org", "acme", "--remote", &uri, "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(json_of(&out)["group"]["parent"], "eng");

    let out = oak(args(&[
        "team",
        "add",
        "@team/web",
        "@dave",
        "--maintainer",
        "--org",
        "acme",
        "--remote",
        &uri,
        "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        json_of(&out)["group"]["maintainers"],
        serde_json::json!(["bob", "dave"])
    );

    let out = oak(args(&[
        "group", "remove", "web", "zed", "--org", "acme", "--remote", &uri,
    ]))
    .await;
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("isn't a member"), "{err}");
}

#[tokio::test]
async fn group_grant_invite_and_my_groups() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/orgs/acme/groups/web/repos/mono"))
        .and(body_partial_json(serde_json::json!({"role": "writer"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "repo": "mono", "is_public": false,
            "grants": [{"group": "web", "kind": "custom", "role": "writer"}],
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/orgs/acme/groups/web/invite"))
        .and(body_partial_json(serde_json::json!({"username": "erin"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "group": {"slug": "web", "kind": "custom", "members": ["erin"]},
            "username": "erin", "joined_org": true,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user/groups"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "username": "erin",
            "groups": [
                {"org": "acme", "slug": "members", "kind": "members", "description": "", "maintainer": false},
                {"org": "acme", "slug": "web", "kind": "custom", "description": "", "maintainer": true},
                {"org": "other", "slug": "ops", "kind": "custom", "description": "", "maintainer": false},
            ],
        })))
        .mount(&server)
        .await;
    let uri = server.uri();

    let out = oak(args(&[
        "group",
        "grant",
        "web",
        "acme/mono",
        "--role",
        "write",
        "--org",
        "acme",
        "--remote",
        &uri,
        "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(json_of(&out)["grants"][0]["role"], "writer");
    let out = oak(args(&[
        "group",
        "grant",
        "web",
        "other/mono",
        "--org",
        "acme",
        "--remote",
        &uri,
    ]))
    .await;
    assert!(!out.status.success(), "a repo of another org");

    let out = oak(args(&[
        "group", "invite", "web", "erin", "--org", "acme", "--remote", &uri,
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("joined acme")
            || String::from_utf8_lossy(&out.stdout).contains("joined acme")
    );

    let out = oak(args(&["groups", "--remote", &uri, "--json"])).await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(json_of(&out)["groups"].as_array().unwrap().len(), 3);
    let out = oak(args(&["groups", "--org", "acme", "--remote", &uri])).await;
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("@group/web  (maintainer)"), "{text}");
    assert!(!text.contains("@group/ops"), "{text}");
}

/// `oak mount ORG/REPO --path DIR`: a view of those directories on a server
/// that serves partial mounts; refused (not silently full) on one that
/// doesn't; and a plain mount needs no server at all.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
#[tokio::test]
async fn mount_paths_become_a_view_only_where_the_server_serves_one() {
    use oak_cli::commands::mount::resolve_mount_target;
    let server = server().await;
    let remote = server.uri();

    let (repo, view, leaf) = resolve_mount_target(&remote, "acme/mono", &[])
        .await
        .unwrap();
    assert_eq!(
        (repo.as_str(), view, leaf.as_str()),
        ("acme/mono", None, "mono")
    );

    let (repo, view, leaf) = resolve_mount_target(
        &remote,
        "acme/mono",
        &["apps/web".into(), "libs/ui,libs/auth".into()],
    )
    .await
    .unwrap();
    assert_eq!(repo, "acme/mono");
    assert_eq!(
        view.as_deref(),
        Some("+apps/web\n+libs/auth\n+libs/ui\n=AGENTS.md\n=CLAUDE.md")
    );
    assert!(leaf.starts_with("mono-"), "{leaf}");

    let err = resolve_mount_target(&remote, "acme/plain", &["src".into()])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("doesn't serve partial mounts"),
        "{err}"
    );
}

/// `oak group agent`: the org's agents (service accounts) on groups.
#[tokio::test]
async fn group_agent_commands_put_agents_on_groups() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/orgs/acme/service-accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"name": "deploy-bot", "display_name": "", "disabled": false,
             "created_at": "2026-11-11T00:00:00Z", "groups": ["web", "ops"]},
            {"name": "nightly", "display_name": "", "disabled": true,
             "created_at": "2026-11-11T00:00:00Z", "groups": []},
        ])))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/orgs/acme/groups/web/agents/deploy-bot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "slug": "web", "parent": null, "description": "", "kind": "custom",
            "members": ["bob"], "maintainers": ["bob"], "agents": ["deploy-bot"],
            "repos": [{"repo": "mono", "role": "writer"}],
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/orgs/acme/groups/web/agents/ghost"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({"error": "No agent named \"ghost\""})),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/orgs/acme/groups/web/agents/deploy-bot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "slug": "web", "parent": null, "description": "", "kind": "custom",
            "members": ["bob"], "maintainers": ["bob"], "agents": [], "repos": [],
        })))
        .mount(&server)
        .await;
    // A server from before agents were on groups: its catch-all only answers GET.
    Mock::given(method("PUT"))
        .and(path("/api/orgs/acme/groups/old/agents/deploy-bot"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    let uri = server.uri();
    let org = ["--org", "acme", "--remote", uri.as_str()];
    let cmd = |parts: &[&str]| {
        let mut all: Vec<&str> = parts.to_vec();
        all.extend_from_slice(&org);
        args(&all)
    };

    let out = oak(cmd(&["group", "agent", "list"])).await;
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("deploy-bot  @group/web, @group/ops"),
        "{text}"
    );
    assert!(
        text.contains("nightly     no groups (public repos only)  disabled"),
        "{text}"
    );
    let out = oak(cmd(&["group", "agent", "list", "--json"])).await;
    assert_eq!(
        json_of(&out)["agents"][0]["groups"],
        serde_json::json!(["web", "ops"])
    );

    let out = oak(cmd(&[
        "group",
        "agent",
        "add",
        "web",
        "deploy-bot",
        "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        json_of(&out)["group"]["agents"],
        serde_json::json!(["deploy-bot"])
    );
    let out = oak(cmd(&["group", "agent", "add", "@group/web", "deploy-bot"])).await;
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("agents: deploy-bot"), "{text}");

    let out = oak(cmd(&[
        "group",
        "agent",
        "remove",
        "web",
        "deploy-bot",
        "--json",
    ]))
    .await;
    assert!(out.status.success(), "{out:?}");
    assert_eq!(json_of(&out)["group"]["agents"], serde_json::json!([]));

    let out = oak(cmd(&["group", "agent", "add", "web", "ghost", "--json"])).await;
    assert!(!out.status.success());
    let body = json_of(&out);
    assert_eq!(body["error"]["code"], "invalid_argument", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No agent named"),
        "{body}"
    );

    let out = oak(cmd(&["group", "agent", "add", "old", "deploy-bot"])).await;
    assert!(!out.status.success());
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("can't put agents on groups yet"), "{all}");
}
