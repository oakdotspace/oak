//! `oak project --org ORG`: the server's project index, read with no
//! checkout. Paged fetches are stitched together, hidden projects are only
//! counted, recommended commands name the org and the server they came from,
//! and an org without a company repo is a clear usage error.

use std::process::{Command, Output};

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn record(name: &str, dir: &str, deps: &[&str], dependents: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "path": dir,
        "description": format!("the {name} project"),
        "maintainers": ["@alice"],
        "writers": null,
        "readers": "org",
        "my_access": "write",
        "deps": deps,
        "dep_closure": [dir],
        "dependents": dependents,
        "dependents_transitive": dependents.len(),
        "workflows": [],
        "files": 3,
        "bytes": 120,
        "last_changed_at": null,
    })
}

async fn index_server() -> MockServer {
    let server = MockServer::builder().start().await;
    let page = |projects: Vec<serde_json::Value>, offset: usize, next: Option<usize>| {
        serde_json::json!({
            "schema_version": 1,
            "org": "acme",
            "repo": "acme/company",
            "head": "a".repeat(64),
            "total": 3,
            "offset": offset,
            "projects": projects,
            "next_offset": next,
            "hidden_projects": 2,
        })
    };
    Mock::given(method("GET"))
        .and(path("/api/orgs/acme/projects"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![
                record("web", "apps/web", &["ui"], &["admin"]),
                record("admin", "apps/web/admin", &["web"], &[]),
            ],
            0,
            Some(2),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/orgs/acme/projects"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![record("ui", "libs/ui", &[], &["web"])],
            2,
            None,
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/orgs/nope/projects"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": "nope has no company repo"
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
async fn project_list_reads_the_server_index_without_a_checkout() {
    let server = index_server().await;
    let uri = server.uri();
    let out = oak(vec![
        "project".into(),
        "list".into(),
        "acme".into(),
        "--json".into(),
        format!("--remote={uri}"),
    ])
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = json_of(&out);
    assert_eq!(body["source"], "remote");
    assert_eq!(body["repo"], "acme/company");
    assert_eq!(body["total"], 3, "both pages: {body}");
    assert_eq!(body["hidden_projects"], 2);
    assert_eq!(body["projects"][2]["name"], "ui");
    assert_eq!(body["projects"][0]["my_access"], "write");
    let next = body["recommended_next_commands"][0].as_str().unwrap();
    assert!(
        next.contains("--org acme") && next.contains(&format!("--remote={uri}")),
        "recommendations reproduce the source: {next}"
    );

    let out = oak(vec![
        "project".into(),
        "list".into(),
        "--org".into(),
        "acme".into(),
        "--json".into(),
        "--fields".into(),
        "name".into(),
        "--limit".into(),
        "1".into(),
        format!("--remote={uri}"),
    ])
    .await;
    let body = json_of(&out);
    assert_eq!(body["projects"], serde_json::json!([{ "name": "web" }]));
    assert!(body["next_page_command"]
        .as_str()
        .unwrap()
        .contains("--offset 1"));
}

#[tokio::test]
async fn project_show_and_which_work_on_the_server_index() {
    let server = index_server().await;
    let remote = format!("--remote={}", server.uri());
    let show = |args: &[&str]| {
        let mut v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        v.push(remote.clone());
        oak(v)
    };
    let body = json_of(&show(&["project", "show", "ui", "--org", "acme", "--json"]).await);
    assert_eq!(body["project"]["path"], "libs/ui");
    assert_eq!(
        body["dependents_closure"],
        serde_json::json!(["admin", "web"]),
        "web directly, admin through web"
    );

    let body = json_of(
        &show(&[
            "project",
            "which",
            "apps/web/admin/x.ts",
            "--org",
            "acme",
            "--json",
        ])
        .await,
    );
    assert_eq!(body["project"]["name"], "admin", "the deepest owner");

    let out = show(&["project", "list", "nope", "--json"]).await;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("no company repo") || stdout.contains("no company repo"),
        "stderr={stderr} stdout={stdout}"
    );
}

/// `oak mount`'s target: `ORG/PROJECT` mounts the project's view of the
/// company repo, `ORG --project A --project B` the union of their views, a
/// plain repo mounts as before, and projects in different repos are refused.
#[tokio::test]
async fn mount_targets_resolve_projects_to_views_of_the_company_repo() {
    use oak_cli::commands::mount::resolve_mount_target;
    let server = MockServer::builder().start().await;
    let resolved = |path: &str, view: &str, repo: &str| serde_json::json!({ "kind": "project", "repo": repo, "path": path, "view": view });
    for (name, body) in [
        (
            "web",
            resolved(
                "apps/web",
                "+apps/web\n+libs/ui\n=AGENTS.md",
                "acme/company",
            ),
        ),
        (
            "api",
            resolved("apps/api", "+apps/api\n=AGENTS.md", "acme/company"),
        ),
        ("ops", resolved("ops", "+ops", "acme/infra")),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/api/orgs/acme/resolve/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    let remote = server.uri();

    let (repo, view, leaf) = resolve_mount_target(&remote, "acme/web", &[])
        .await
        .unwrap();
    assert_eq!(repo, "acme/company");
    assert_eq!(view.as_deref(), Some("+apps/web\n+libs/ui\n=AGENTS.md"));
    assert_eq!(leaf, "web");

    let (repo, view, leaf) = resolve_mount_target(&remote, "acme", &["web".into(), "api".into()])
        .await
        .unwrap();
    assert_eq!(repo, "acme/company");
    assert_eq!(
        view.as_deref(),
        Some("+apps/api\n+apps/web\n+libs/ui\n=AGENTS.md")
    );
    assert_eq!(leaf, "web-api");

    // Not a project (404 here): an ordinary repo mount.
    let (repo, view, _) = resolve_mount_target(&remote, "acme/tools", &[])
        .await
        .unwrap();
    assert_eq!((repo.as_str(), view), ("acme/tools", None));

    let err = resolve_mount_target(&remote, "acme", &["web".into(), "ops".into()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("different repos"), "{err}");
    let err = resolve_mount_target(&remote, "acme", &["nope".into()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no project named"), "{err}");
}
