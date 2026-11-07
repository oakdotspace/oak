//! `oak project` — discover the projects of a company repo.
//!
//! A project is a directory with an `OAK_PROJECT` file at its root (see
//! `oak_core::project`). These commands answer the agent questions from the
//! projects design: what exists (`list`, `search`), what one project is
//! (`show`), what it needs and what needs it (`deps`), and who owns a path
//! (`which`).
//!
//! Two sources, one output shape:
//!
//! - **Local** (the default inside a checkout): the `OAK_PROJECT` files
//!   committed at HEAD, offline. A project file with no local content (outside
//!   a sparse cone, or withheld under path permissions) is listed under
//!   `unavailable` rather than guessed at. Fields that need the server
//!   (`my_access`, `bytes`, `last_changed_at`) are absent.
//! - **Remote** (`--org ORG`, or `oak project list ORG`): the server's
//!   permission-filtered index of the org's company repo at `main`
//!   (`GET /api/orgs/:org/projects`). No checkout needed. Projects the caller
//!   can't read are only counted (`hidden_projects`).
//!
//! Both produce the same per-project records, so every subcommand works the
//! same way on either.

use std::collections::BTreeSet;
use std::path::Path;

use oak_core::project::{Dep, Problem, Project, ProjectFile, Readers, Severity};
use oak_core::{
    parse_project_file, OakError, ProjectIndex, Repository, Result, SqliteRepository, PROJECT_FILE,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::output;

pub const SCHEMA_VERSION: u32 = 1;

/// Default page size for `oak project list --json`.
const DEFAULT_LIMIT: usize = 200;
/// Page size and page cap when fetching a whole remote index.
const REMOTE_PAGE: usize = 1_000;
const REMOTE_MAX_PAGES: usize = 100;
/// Bound on one remote index page.
const REMOTE_PAGE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// What `oak project` should do. Mirrors the clap subcommands in `main.rs`.
pub enum ProjectAction {
    List {
        fields: Option<String>,
        limit: Option<usize>,
        offset: usize,
    },
    Show {
        name: String,
    },
    Which {
        path: String,
    },
    Deps {
        name: String,
    },
    Search {
        term: String,
    },
    /// The projects the current branch touches and who depends on them.
    Impact,
}

/// Where the index comes from.
pub enum Source {
    /// The current checkout's HEAD.
    Local,
    /// The server's index for `org` on `remote`.
    Remote { remote: String, org: String },
}

// ---------------------------------------------------------------------------
// The local index
// ---------------------------------------------------------------------------

/// The project index at HEAD plus what couldn't be read.
pub struct LocalProjects {
    pub head: Option<String>,
    pub index: ProjectIndex,
    pub problems: Vec<Problem>,
    /// Project directories whose `OAK_PROJECT` has no local content.
    pub unavailable: Vec<String>,
    /// Every committed path, for `files` counts and workflow discovery.
    paths: Vec<String>,
}

fn is_project_file(path: &str) -> bool {
    path == PROJECT_FILE
        || path
            .strip_suffix(PROJECT_FILE)
            .is_some_and(|dir| dir.ends_with('/'))
}

/// Build the index from the `OAK_PROJECT` files committed at HEAD.
pub fn load_local(repo: &dyn Repository) -> Result<LocalProjects> {
    let Some(head) = repo.get_head()? else {
        return Ok(LocalProjects {
            head: None,
            index: ProjectIndex::default(),
            problems: Vec::new(),
            unavailable: Vec::new(),
            paths: Vec::new(),
        });
    };
    let commit = repo
        .get_commit(&head)?
        .ok_or_else(|| OakError::CommitNotFound(head.to_string()))?;
    let manifest = repo
        .get_manifest(&commit.manifest_hash)?
        .ok_or_else(|| OakError::ManifestNotFound(commit.manifest_hash.to_string()))?;
    let mut files = Vec::new();
    let mut problems = Vec::new();
    let mut unavailable = Vec::new();
    for entry in manifest.entries.iter().filter(|e| is_project_file(&e.path)) {
        let dir = entry
            .path
            .strip_suffix(PROJECT_FILE)
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        match repo.get_blob(&entry.blob_hash)? {
            Some(blob) => {
                let (file, found) = parse_project_file(&dir, &blob.content);
                problems.extend(found);
                if let Some(file) = file {
                    files.push((dir, file));
                }
            }
            None => unavailable.push(dir),
        }
    }
    let (index, found) = ProjectIndex::build(files);
    problems.extend(found);
    Ok(LocalProjects {
        head: Some(head.to_string()),
        index,
        problems,
        unavailable,
        paths: manifest.entries.into_iter().map(|e| e.path).collect(),
    })
}

fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

/// Does `dependent` list a dep that points at `target`: a dep path `target`
/// owns (it is the deepest project covering it), or one covering `target`'s
/// whole directory? The same rule as the server's index, so the root project
/// isn't everyone's dependency just because it encloses everything.
fn depends_on(index: &ProjectIndex, dependent: &Project, target: &Project) -> bool {
    dependent.file.name != target.file.name
        && index.direct_deps(&dependent.file.name).is_some_and(|deps| {
            deps.iter().any(|d| {
                covers(d, &target.path) || index.owner_of(d).is_some_and(|o| o.path == target.path)
            })
        })
}

/// One project's record. The remote index serves the same shape (plus
/// `my_access`, `bytes`, `last_changed_at`).
#[derive(Serialize, Clone)]
pub struct ProjectJson {
    pub name: String,
    /// The project directory, repo-relative (`""` for the root project).
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub maintainers: Vec<String>,
    /// `null`: everyone who can read may write.
    pub writers: Option<Vec<String>>,
    /// `"org"`, or the extra readers of a restricted project.
    pub readers: Value,
    /// Direct deps as written: project names, or `/path` prefixes.
    pub deps: Vec<String>,
    /// Every path prefix the project needs: its own directory plus
    /// transitive deps.
    pub dep_closure: Vec<String>,
    /// Projects that depend on this one directly.
    pub dependents: Vec<String>,
    /// How many projects depend on it, directly or transitively.
    pub dependents_transitive: usize,
    /// `.oak/workflows/*.yml` files of the project.
    pub workflows: Vec<String>,
    /// Committed files the project owns (a nested project's files count
    /// toward the nested project).
    pub files: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub self_approve: bool,
}

fn dep_label(dep: &Dep) -> String {
    match dep {
        Dep::Project(name) => name.clone(),
        Dep::Path(path) => format!("/{path}"),
    }
}

fn readers_json(file: &ProjectFile) -> Value {
    match &file.readers {
        Readers::Org => Value::String("org".to_string()),
        Readers::Restricted(extra) => {
            Value::Array(extra.iter().map(|p| p.to_string().into()).collect())
        }
    }
}

impl LocalProjects {
    pub fn project_json(&self, project: &Project) -> ProjectJson {
        let file = &project.file;
        let workflows_dir = if project.path.is_empty() {
            ".oak/workflows/".to_string()
        } else {
            format!("{}/.oak/workflows/", project.path)
        };
        let dependents: Vec<String> = self
            .index
            .projects()
            .iter()
            .filter(|d| depends_on(&self.index, d, project))
            .map(|d| d.file.name.clone())
            .collect();
        let owns = |p: &str| {
            self.index
                .owner_of(p)
                .is_some_and(|o| o.path == project.path)
        };
        ProjectJson {
            name: file.name.clone(),
            path: project.path.clone(),
            description: file.description.clone(),
            maintainers: file.maintainers.iter().map(ToString::to_string).collect(),
            writers: file
                .writers
                .as_ref()
                .map(|w| w.iter().map(ToString::to_string).collect()),
            readers: readers_json(file),
            deps: file.deps.iter().map(dep_label).collect(),
            dep_closure: self.index.dep_closure(&[file.name.as_str()]),
            dependents_transitive: 0,
            dependents,
            workflows: self
                .paths
                .iter()
                .filter(|p| {
                    p.strip_prefix(&workflows_dir).is_some_and(|rest| {
                        !rest.contains('/') && (rest.ends_with(".yml") || rest.ends_with(".yaml"))
                    })
                })
                .cloned()
                .collect(),
            files: self.paths.iter().filter(|p| owns(p)).count(),
            self_approve: file.self_approve,
        }
    }

    fn into_index(self) -> Result<Index> {
        let mut records = self
            .index
            .projects()
            .iter()
            .map(|p| serde_json::to_value(self.project_json(p)))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        fill_transitive_counts(&mut records);
        let problems = problems_json(&self.problems);
        Ok(Index {
            source: "local_head",
            remote: None,
            head: self.head,
            org: None,
            repo: None,
            records,
            hidden_projects: 0,
            unavailable: self.unavailable,
            problems,
        })
    }
}

#[derive(Serialize)]
struct ProblemJson {
    severity: &'static str,
    /// The project directory the problem is about.
    path: String,
    message: String,
}

fn problems_json(problems: &[Problem]) -> Vec<ProblemJson> {
    problems
        .iter()
        .map(|p| ProblemJson {
            severity: match p.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            },
            path: p.path.clone(),
            message: p.message.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The remote index
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RemotePage {
    org: String,
    repo: String,
    head: String,
    total: usize,
    projects: Vec<Value>,
    #[serde(default)]
    next_offset: Option<usize>,
    #[serde(default)]
    hidden_projects: usize,
}

async fn fetch_remote_page(
    remote: &str,
    org: &str,
    query: &[(&str, String)],
) -> Result<RemotePage> {
    let client = crate::http::api_client();
    let timeout = crate::http::read_only_probe_timeout();
    let mut request = client
        .get(format!("{remote}/api/orgs/{org}/projects"))
        .query(query)
        .timeout(timeout);
    if let Some(token) = super::credentials::effective_token(remote) {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let shown = crate::http::redact_url_userinfo(remote);
    let transport = |error: reqwest::Error| {
        OakError::Http(format!(
            "project index from {shown}: {}",
            crate::http::transport_failure_reason(&error, timeout)
        ))
    };
    let mut resp = request.send().await.map_err(transport)?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(OakError::AuthRequired {
            remote: shown.clone(),
            status: status.as_u16(),
        });
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(OakError::InvalidArgument(format!(
            "{org} has no company repo you can read on {shown} (or the server has no project index)"
        )));
    }
    if !status.is_success() {
        return Err(crate::http::server_error(resp).await);
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport)? {
        if chunk.len() > REMOTE_PAGE_MAX_BYTES.saturating_sub(body.len()) {
            return Err(OakError::Server(format!(
                "project index page exceeds the {REMOTE_PAGE_MAX_BYTES}-byte client bound"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| OakError::Server("the server returned an unreadable project index".into()))
}

/// The whole remote index, page by page.
async fn fetch_remote(remote: &str, org: &str) -> Result<Index> {
    let mut records = Vec::new();
    let mut offset = 0;
    for _ in 0..REMOTE_MAX_PAGES {
        let page = fetch_remote_page(
            remote,
            org,
            &[
                ("limit", REMOTE_PAGE.to_string()),
                ("offset", offset.to_string()),
            ],
        )
        .await?;
        records.extend(page.projects);
        match page.next_offset {
            Some(next) if next > offset && records.len() < page.total => offset = next,
            _ => {
                return Ok(Index {
                    source: "remote",
                    remote: Some(crate::http::redact_url_userinfo(remote)),
                    head: Some(page.head),
                    org: Some(page.org),
                    repo: Some(page.repo),
                    records,
                    hidden_projects: page.hidden_projects,
                    unavailable: Vec::new(),
                    problems: Vec::new(),
                });
            }
        }
    }
    Err(OakError::Server(format!(
        "project index for {org} has more than {} projects",
        REMOTE_PAGE * REMOTE_MAX_PAGES
    )))
}

// ---------------------------------------------------------------------------
// Records, whatever their source
// ---------------------------------------------------------------------------

struct Index {
    source: &'static str,
    /// The server a remote index came from (no userinfo).
    remote: Option<String>,
    head: Option<String>,
    org: Option<String>,
    repo: Option<String>,
    /// One JSON record per project, sorted by path.
    records: Vec<Value>,
    hidden_projects: usize,
    unavailable: Vec<String>,
    problems: Vec<ProblemJson>,
}

fn str_field<'a>(record: &'a Value, key: &str) -> &'a str {
    record[key].as_str().unwrap_or_default()
}

fn str_list(record: &Value, key: &str) -> Vec<String> {
    record[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Everything that depends on `name`, directly or transitively, by
/// following each record's direct `dependents`.
fn dependents_closure(records: &[Value], name: &str) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut frontier = vec![name.to_string()];
    while let Some(next) = frontier.pop() {
        let Some(record) = records.iter().find(|r| str_field(r, "name") == next) else {
            continue;
        };
        for d in str_list(record, "dependents") {
            if d != name && seen.insert(d.clone()) {
                frontier.push(d);
            }
        }
    }
    seen.into_iter().collect()
}

fn fill_transitive_counts(records: &mut [Value]) {
    let counts: Vec<usize> = records
        .iter()
        .map(|r| dependents_closure(records, str_field(r, "name")).len())
        .collect();
    for (record, count) in records.iter_mut().zip(counts) {
        record["dependents_transitive"] = count.into();
    }
}

impl Index {
    fn get(&self, name: &str) -> Result<&Value> {
        self.records
            .iter()
            .find(|r| str_field(r, "name") == name)
            .ok_or_else(|| {
                OakError::InvalidArgument(format!(
                    "no project named {name:?} (see `oak project list`)"
                ))
            })
    }

    /// The deepest project whose directory covers `path`.
    fn owner_of(&self, path: &str) -> Option<&Value> {
        self.records
            .iter()
            .filter(|r| covers(str_field(r, "path"), path))
            .max_by_key(|r| str_field(r, "path").len())
    }

    /// The flags that reproduce this source in a recommended command: the
    /// org and the server it was read from (without the server, the command
    /// would read the default remote instead).
    fn org_flag(&self) -> String {
        match (&self.org, &self.remote) {
            (Some(org), Some(remote)) => format!(
                " --org {} --remote={}",
                output::shell_quote(org),
                output::shell_quote(remote)
            ),
            _ => String::new(),
        }
    }
}

/// Keep only `fields` (comma-separated) of each record.
fn select_fields(records: Vec<Value>, fields: Option<&str>) -> Vec<Value> {
    let Some(fields) = fields else {
        return records;
    };
    let wanted: BTreeSet<&str> = fields
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    records
        .into_iter()
        .map(|r| match r {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .filter(|(k, _)| wanted.contains(k.as_str()))
                    .collect(),
            ),
            other => other,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// JSON shapes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Header {
    schema_version: u32,
    /// `local_head` or `remote`.
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    org: Option<String>,
    /// `owner/name` of the company repo (remote only).
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
}

impl Index {
    fn header(&self) -> Header {
        Header {
            schema_version: SCHEMA_VERSION,
            source: self.source,
            head: self.head.clone(),
            org: self.org.clone(),
            repo: self.repo.clone(),
        }
    }
}

#[derive(Serialize)]
struct ListJson<'a> {
    #[serde(flatten)]
    header: Header,
    total: usize,
    offset: usize,
    projects: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_page_command: Option<String>,
    /// Projects you can't read (remote only). Never named.
    #[serde(skip_serializing_if = "is_zero")]
    hidden_projects: usize,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    unavailable: &'a [String],
    #[serde(skip_serializing_if = "<[ProblemJson]>::is_empty")]
    problems: &'a [ProblemJson],
    recommended_next_commands: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Serialize)]
struct ShowJson<'a> {
    #[serde(flatten)]
    header: Header,
    project: &'a Value,
    /// Every project that depends on this one, directly or transitively.
    dependents_closure: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<&'a ProblemJson>,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct WhichJson<'a> {
    #[serde(flatten)]
    header: Header,
    path: String,
    /// The deepest project whose directory covers `path`; absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a Value>,
    recommended_next_commands: Vec<String>,
}

#[derive(Serialize)]
struct DepsJson {
    #[serde(flatten)]
    header: Header,
    name: String,
    deps: Vec<String>,
    dep_closure: Vec<String>,
    dependents: Vec<String>,
    dependents_closure: Vec<String>,
    recommended_next_commands: Vec<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

pub fn run(
    cwd: &Path,
    rt: &tokio::runtime::Runtime,
    source: Source,
    action: ProjectAction,
    json: bool,
) -> Result<()> {
    let (index, work_tree) = match source {
        Source::Local => {
            let ctx = crate::resolve::resolve(cwd)?;
            let repo = SqliteRepository::open(&ctx.db_path()?)?;
            (load_local(&repo)?.into_index()?, Some(ctx.work_tree))
        }
        Source::Remote { remote, org } => {
            let remote = remote.trim_end_matches('/').to_string();
            (rt.block_on(fetch_remote(&remote, &org))?, None)
        }
    };
    match action {
        ProjectAction::List {
            fields,
            limit,
            offset,
        } => list(&index, fields.as_deref(), limit, offset, json),
        ProjectAction::Search { term } => search(&index, &term, json),
        ProjectAction::Show { name } => show(&index, &name, json),
        ProjectAction::Deps { name } => deps(&index, &name, json),
        ProjectAction::Impact => {
            let Some(root) = work_tree else {
                return Err(OakError::InvalidArgument(
                    "`oak project impact` reads the current branch: run it in a checkout"
                        .to_string(),
                ));
            };
            impact_command(rt, &root, json)
        }
        ProjectAction::Which { path } => {
            let rel = match &work_tree {
                Some(root) => repo_relative(root, cwd, &path),
                None => path.replace('\\', "/").trim_matches('/').to_string(),
            };
            which(&index, &rel, json)
        }
    }
}

/// A path argument as a repo-relative manifest path: relative to `cwd` when
/// it names something there, else taken as already repo-relative.
fn repo_relative(work_tree: &Path, cwd: &Path, path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let joined = cwd.join(&normalized);
    let joined = joined.canonicalize().unwrap_or(joined);
    let root = work_tree
        .canonicalize()
        .unwrap_or_else(|_| work_tree.to_path_buf());
    match joined.strip_prefix(&root) {
        Ok(rel) if cwd.join(&normalized).exists() => rel.to_string_lossy().replace('\\', "/"),
        _ => normalized.trim_matches('/').to_string(),
    }
}

fn print_rows<'a>(records: impl IntoIterator<Item = &'a Value>) {
    for r in records {
        let path = match str_field(r, "path") {
            "" => ".",
            p => p,
        };
        output::print_line(&format!(
            "{:<24} {:<32} {}",
            str_field(r, "name"),
            path,
            str_field(r, "description")
        ));
    }
}

fn list(
    index: &Index,
    fields: Option<&str>,
    limit: Option<usize>,
    offset: usize,
    json: bool,
) -> Result<()> {
    if !json {
        if index.records.is_empty() {
            output::info("No projects (no OAK_PROJECT files).");
        }
        print_rows(&index.records);
        if index.hidden_projects > 0 {
            output::info(&format!(
                "{} more project(s) you can't read.",
                index.hidden_projects
            ));
        }
        print_text_problems(index);
        return Ok(());
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let page: Vec<Value> = index
        .records
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();
    let next = offset + page.len();
    let org_flag = index.org_flag();
    let next_page_command = (next < index.records.len()).then(|| {
        let mut cmd = format!("oak project list --json{org_flag} --offset {next} --limit {limit}");
        if let Some(fields) = fields {
            cmd.push_str(&format!(" --fields {}", output::shell_quote(fields)));
        }
        cmd
    });
    let recommended_next_commands = page
        .first()
        .map(|p| {
            vec![format!(
                "oak project show {} --json{org_flag}",
                output::shell_quote(str_field(p, "name"))
            )]
        })
        .unwrap_or_default();
    output::print_json(&ListJson {
        header: index.header(),
        total: index.records.len(),
        offset,
        projects: select_fields(page, fields),
        next_page_command,
        hidden_projects: index.hidden_projects,
        unavailable: &index.unavailable,
        problems: &index.problems,
        recommended_next_commands,
    })
}

fn search(index: &Index, term: &str, json: bool) -> Result<()> {
    let needle = term.to_lowercase();
    let hits: Vec<&Value> = index
        .records
        .iter()
        .filter(|r| {
            ["name", "path", "description"]
                .iter()
                .any(|k| str_field(r, k).to_lowercase().contains(&needle))
        })
        .collect();
    if !json {
        if hits.is_empty() {
            output::info(&format!("No project matches {term:?}."));
        }
        print_rows(hits);
        return Ok(());
    }
    let org_flag = index.org_flag();
    let recommended_next_commands = hits
        .first()
        .map(|p| {
            vec![format!(
                "oak project show {} --json{org_flag}",
                output::shell_quote(str_field(p, "name"))
            )]
        })
        .unwrap_or_default();
    output::print_json(&ListJson {
        header: index.header(),
        total: hits.len(),
        offset: 0,
        projects: hits.into_iter().cloned().collect(),
        next_page_command: None,
        hidden_projects: index.hidden_projects,
        unavailable: &index.unavailable,
        problems: &[],
        recommended_next_commands,
    })
}

fn show(index: &Index, name: &str, json: bool) -> Result<()> {
    let record = index.get(name)?;
    let closure = dependents_closure(&index.records, name);
    let path = str_field(record, "path");
    let problems: Vec<&ProblemJson> = index.problems.iter().filter(|p| p.path == path).collect();
    if !json {
        output::print_line(&format!(
            "{name} ({})",
            if path.is_empty() { "." } else { path }
        ));
        if !str_field(record, "description").is_empty() {
            output::print_line(&format!("  {}", str_field(record, "description")));
        }
        let line = |label: &str, value: String| {
            output::print_line(&format!("  {label:<12} {value}"));
        };
        line("maintainers:", str_list(record, "maintainers").join(", "));
        line(
            "writers:",
            match record["writers"].as_array() {
                Some(_) => str_list(record, "writers").join(", "),
                None => "everyone who can read".to_string(),
            },
        );
        line(
            "readers:",
            match &record["readers"] {
                Value::String(s) => s.clone(),
                _ => str_list(record, "readers").join(", "),
            },
        );
        if let Some(access) = record["my_access"].as_str() {
            line("my access:", access.to_string());
        }
        line("deps:", str_list(record, "deps").join(", "));
        line("needs:", str_list(record, "dep_closure").join(", "));
        line("dependents:", closure.join(", "));
        line("files:", record["files"].to_string());
        for p in &problems {
            output::warning(&p.message);
        }
        return Ok(());
    }
    output::print_json(&ShowJson {
        header: index.header(),
        recommended_next_commands: vec![format!(
            "oak project deps {} --json{}",
            output::shell_quote(name),
            index.org_flag()
        )],
        project: record,
        dependents_closure: closure,
        problems,
    })
}

fn deps(index: &Index, name: &str, json: bool) -> Result<()> {
    let record = index.get(name)?;
    let closure = dependents_closure(&index.records, name);
    if !json {
        output::print_line(&format!(
            "{name} needs: {}",
            str_list(record, "dep_closure").join(", ")
        ));
        output::print_line(&format!(
            "depends on {name}: {}",
            if closure.is_empty() {
                "(nothing)".to_string()
            } else {
                closure.join(", ")
            }
        ));
        return Ok(());
    }
    output::print_json(&DepsJson {
        header: index.header(),
        name: name.to_string(),
        recommended_next_commands: closure
            .first()
            .map(|d| {
                vec![format!(
                    "oak project show {} --json{}",
                    output::shell_quote(d),
                    index.org_flag()
                )]
            })
            .unwrap_or_default(),
        deps: str_list(record, "deps"),
        dep_closure: str_list(record, "dep_closure"),
        dependents: str_list(record, "dependents"),
        dependents_closure: closure,
    })
}

fn which(index: &Index, path: &str, json: bool) -> Result<()> {
    let owner = index.owner_of(path);
    if !json {
        match owner {
            Some(p) => {
                let maintainers = str_list(p, "maintainers");
                let dir = match str_field(p, "path") {
                    "" => ".",
                    d => d,
                };
                output::print_line(&format!(
                    "{path}: {} ({dir}) — maintainers: {}",
                    str_field(p, "name"),
                    if maintainers.is_empty() {
                        "org owners/admins".to_string()
                    } else {
                        maintainers.join(", ")
                    }
                ));
            }
            None => output::info(&format!("{path}: no project owns this path.")),
        }
        return Ok(());
    }
    output::print_json(&WhichJson {
        header: index.header(),
        path: path.to_string(),
        recommended_next_commands: owner
            .map(|p| {
                vec![format!(
                    "oak project show {} --json{}",
                    output::shell_quote(str_field(p, "name")),
                    index.org_flag()
                )]
            })
            .unwrap_or_default(),
        project: owner,
    })
}

fn print_text_problems(index: &Index) {
    for dir in &index.unavailable {
        let path = if dir.is_empty() { "." } else { dir.as_str() };
        output::warning(&format!(
            "{path}/{PROJECT_FILE} has no local content (outside the sparse cone or withheld)"
        ));
    }
    for p in &index.problems {
        output::warning(&format!(
            "{}: {}",
            if p.path.is_empty() { "." } else { &p.path },
            p.message
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(files: &[(&str, &str)], paths: &[&str]) -> LocalProjects {
        let parsed = files.iter().map(|(dir, text)| {
            (
                dir.to_string(),
                parse_project_file(dir, text.as_bytes()).0.unwrap(),
            )
        });
        let (index, problems) = ProjectIndex::build(parsed);
        LocalProjects {
            head: None,
            index,
            problems,
            unavailable: Vec::new(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn fixture() -> Index {
        local(
            &[
                ("", "name = \"acme\"\nmaintainers = [\"@root\"]"),
                (
                    "apps/web",
                    "name = \"web\"\ndescription = \"Customer web app\"\ndeps = [\"ui\"]\nmaintainers = [\"@alice\"]",
                ),
                ("libs/ui", "name = \"ui\"\ndeps = [\"/libs/tokens\"]"),
                ("apps/admin", "name = \"admin\"\ndeps = [\"web\"]"),
            ],
            &[
                "OAK_PROJECT",
                "README.md",
                "apps/web/OAK_PROJECT",
                "apps/web/main.ts",
                "apps/web/.oak/workflows/ci.yml",
                "apps/web/.oak/workflows/nested/x.yml",
                "libs/ui/OAK_PROJECT",
                "libs/ui/button.tsx",
                "libs/tokens/colors.json",
                "apps/admin/OAK_PROJECT",
            ],
        )
        .into_index()
        .unwrap()
    }

    #[test]
    fn records_carry_deps_dependents_and_workflows() {
        let index = fixture();
        let web = index.get("web").unwrap();
        assert_eq!(str_list(web, "deps"), vec!["ui"]);
        assert_eq!(
            str_list(web, "dep_closure"),
            vec!["apps/web", "libs/tokens", "libs/ui"]
        );
        assert_eq!(str_list(web, "dependents"), vec!["admin"]);
        assert_eq!(web["dependents_transitive"], 1);
        assert_eq!(
            str_list(web, "workflows"),
            vec!["apps/web/.oak/workflows/ci.yml"]
        );
        assert_eq!(web["files"], 4);
        assert_eq!(web["readers"], "org");

        let ui = index.get("ui").unwrap();
        assert_eq!(str_list(ui, "deps"), vec!["/libs/tokens"]);
        assert_eq!(str_list(ui, "dependents"), vec!["web"]);
        assert_eq!(ui["dependents_transitive"], 2, "web and, through it, admin");
        assert_eq!(
            dependents_closure(&index.records, "ui"),
            vec!["admin", "web"]
        );
    }

    #[test]
    fn the_root_is_not_everyones_dependency() {
        let index = fixture();
        let acme = index.get("acme").unwrap();
        // ui depends on /libs/tokens, which the root owns — that one counts.
        assert_eq!(str_list(acme, "dependents"), vec!["ui"]);
        // The root owns only what no nested project does.
        assert_eq!(
            acme["files"], 3,
            "OAK_PROJECT, README.md, libs/tokens/colors.json"
        );
    }

    #[test]
    fn which_picks_the_deepest_owner() {
        let index = fixture();
        assert_eq!(
            str_field(index.owner_of("apps/web/main.ts").unwrap(), "name"),
            "web"
        );
        assert_eq!(
            str_field(index.owner_of("apps/other/x").unwrap(), "name"),
            "acme"
        );
        assert_eq!(
            str_field(index.owner_of("apps/webby/x").unwrap(), "name"),
            "acme"
        );
    }

    #[test]
    fn fields_select_a_subset() {
        let index = fixture();
        let picked = select_fields(vec![index.get("web").unwrap().clone()], Some("name, path"));
        assert_eq!(
            picked[0],
            serde_json::json!({ "name": "web", "path": "apps/web" })
        );
    }

    #[test]
    fn project_files_are_recognized() {
        assert!(is_project_file("OAK_PROJECT"));
        assert!(is_project_file("a/b/OAK_PROJECT"));
        assert!(!is_project_file("a/NOT_OAK_PROJECT"));
    }
}

// ---------------------------------------------------------------------------
// Name resolution (`oak clone ORG/PROJECT`)
// ---------------------------------------------------------------------------

/// What `org/name` is on a server that has a project index.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// `repo` or `project`.
    pub kind: String,
    /// `owner/name` of the repo holding it.
    pub repo: String,
    /// The project directory (`""` for a repo).
    #[serde(default)]
    pub path: String,
    /// The project's view (canonical spec); absent for a repo.
    #[serde(default)]
    pub view: Option<String>,
}

/// Resolve `spec` (`org/name`) through `GET /api/orgs/:org/resolve/:name`.
/// `None` for anything that isn't a project the caller can read — a repo, an
/// unknown name, or a server without the endpoint — so the caller carries on
/// exactly as before.
pub async fn resolve_project(remote: &str, spec: &str) -> Option<Resolved> {
    let (org, name) = spec.trim().split_once('/')?;
    if org.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    let mut url = reqwest::Url::parse(remote.trim_end_matches('/')).ok()?;
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["api", "orgs", org, "resolve", name]);
    let mut request = crate::http::api_client()
        .get(url)
        .timeout(crate::http::read_only_probe_timeout());
    if let Some(token) = super::credentials::effective_token(remote) {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let resp = request.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let resolved: Resolved = resp.json().await.ok()?;
    (resolved.kind == "project" && resolved.view.is_some()).then_some(resolved)
}

/// Mark the checkout at `work_tree` as holding `view`, so every later command
/// there sends it.
pub fn record_view(work_tree: &Path, view: &crate::http::ActiveView) -> Result<()> {
    let ctx = crate::resolve::resolve(work_tree)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    repo.set_metadata(oak_core::MetadataKey::ViewSpec, &view.spec)
}

// ---------------------------------------------------------------------------
// Impact: what a branch touches and who depends on it
// ---------------------------------------------------------------------------

/// The blast radius of a set of changed paths: the projects owning them and
/// every project that depends on those, directly or transitively. The same
/// block rides on `oak status --json`, `oak finish --json` and
/// `oak merge --json`.
#[derive(Serialize, Debug, Clone)]
pub struct ImpactJson {
    /// Where the project index came from: `local_head` (this checkout's
    /// `OAK_PROJECT` files) or `remote` (the server's index, which also knows
    /// the projects outside a view).
    pub source: &'static str,
    pub touched_projects: Vec<TouchedProjectJson>,
    /// Projects that depend on a touched one but aren't touched themselves:
    /// places the branch could break that the agent may not see.
    pub dependents_affected: Vec<DependentJson>,
    /// Projects still needing an approval, as far as the server said.
    pub missing_approvals: Vec<String>,
    /// The server gates this branch on approvals. `false` also when it wasn't
    /// asked (`oak status`) or didn't answer; `caveats` says so.
    pub approvals_enforced: bool,
    /// Each touched project's approval state, when the server answered.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<Value>,
    /// Workflow files of the touched projects (what CI will run).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct TouchedProjectJson {
    pub name: String,
    pub path: String,
    pub maintainers: Vec<String>,
    pub changed_files: usize,
}

#[derive(Serialize, Debug, Clone)]
pub struct DependentJson {
    pub name: String,
    pub path: String,
    pub maintainers: Vec<String>,
    /// The touched projects it depends on (directly or transitively).
    pub depends_on: Vec<String>,
}

/// The impact of `paths` over `index`; `None` when the repo has no projects.
fn impact_over<'a>(index: &Index, paths: impl IntoIterator<Item = &'a str>) -> Option<ImpactJson> {
    if index.records.is_empty() {
        return None;
    }
    let mut touched: std::collections::BTreeMap<String, usize> = Default::default();
    for path in paths {
        if let Some(owner) = index.owner_of(path) {
            *touched
                .entry(str_field(owner, "name").to_string())
                .or_default() += 1;
        }
    }
    let mut dependents: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    for name in touched.keys() {
        for d in dependents_closure(&index.records, name) {
            if !touched.contains_key(&d) {
                dependents.entry(d).or_default().insert(name.clone());
            }
        }
    }
    let record = |name: &str| index.records.iter().find(|r| str_field(r, "name") == name);
    let touched_projects: Vec<TouchedProjectJson> = touched
        .iter()
        .filter_map(|(name, &changed_files)| {
            let r = record(name)?;
            Some(TouchedProjectJson {
                name: name.clone(),
                path: str_field(r, "path").to_string(),
                maintainers: str_list(r, "maintainers"),
                changed_files,
            })
        })
        .collect();
    let workflows = touched
        .keys()
        .filter_map(|name| record(name))
        .flat_map(|r| str_list(r, "workflows"))
        .collect();
    let dependents_affected = dependents
        .into_iter()
        .filter_map(|(name, via)| {
            let r = record(&name)?;
            Some(DependentJson {
                path: str_field(r, "path").to_string(),
                maintainers: str_list(r, "maintainers"),
                name,
                depends_on: via.into_iter().collect(),
            })
        })
        .collect();
    Some(ImpactJson {
        source: index.source,
        touched_projects,
        dependents_affected,
        missing_approvals: Vec::new(),
        approvals_enforced: false,
        approvals: Vec::new(),
        workflows,
        caveats: Vec::new(),
    })
}

/// The server's project index for a view checkout's org, or why it couldn't
/// be read. See [`server_index_for`].
pub struct ServerIndex {
    index: Option<Index>,
    caveats: Vec<String>,
}

/// Where a view checkout's server index lives: `(remote, org)`. `None` for a
/// full checkout, whose local index already knows every project.
pub fn server_index_source(repo: &dyn Repository) -> Option<(String, String)> {
    let view = crate::http::view_of_checkout(repo)?;
    let remote = repo
        .get_metadata(oak_core::MetadataKey::RemoteUrl)
        .ok()
        .flatten()?;
    let org = view.repo.split('/').next().unwrap_or_default().to_string();
    Some((remote.trim_end_matches('/').to_string(), org))
}

/// Fetch the server index from [`server_index_source`]; a failure becomes a
/// caveat, not an error.
pub async fn server_index_for((remote, org): (String, String)) -> ServerIndex {
    match fetch_remote(&remote, &org).await {
        Ok(index) => ServerIndex {
            index: Some(index),
            caveats: Vec::new(),
        },
        Err(e) => ServerIndex {
            index: None,
            caveats: vec![format!(
                "the server's project index is unavailable ({e}); dependents outside this view aren't listed"
            )],
        },
    }
}

impl ServerIndex {
    /// The project owning `path` (the deepest one covering it) and its
    /// directory, when the index could be read.
    pub fn owner_of(&self, path: &str) -> Option<(String, String)> {
        let record = self.index.as_ref()?.owner_of(path)?;
        Some((
            str_field(record, "name").to_string(),
            str_field(record, "path").to_string(),
        ))
    }
}

/// The impact of `paths` in the checkout `repo`, from `server`'s index when
/// given (a view checkout: it also knows the projects outside the view) and
/// this checkout's own index otherwise. `None` when the repo has no projects
/// or the index can't be read.
pub fn impact_for(
    server: Option<ServerIndex>,
    repo: &dyn Repository,
    paths: &[String],
) -> Option<ImpactJson> {
    let (remote_index, caveats) = match server {
        Some(server) => (server.index, server.caveats),
        None if crate::http::view_of_checkout(repo).is_some() => (
            None,
            vec![
                "dependents outside this view aren't listed; `oak project impact --json` asks the server"
                    .to_string(),
            ],
        ),
        None => (None, Vec::new()),
    };
    let index = match remote_index {
        Some(index) => index,
        None => load_local(repo).ok()?.into_index().ok()?,
    };
    let mut impact = impact_over(&index, paths.iter().map(String::as_str))?;
    impact.caveats = caveats;
    Some(impact)
}

/// Which branch of which repo a checkout's approvals live on:
/// `(remote, owner, repo, branch)`.
pub fn approvals_source(repo: &dyn Repository) -> Option<(String, String, String, String)> {
    use oak_core::MetadataKey;
    Some((
        repo.get_metadata(MetadataKey::RemoteUrl).ok().flatten()?,
        repo.get_metadata(MetadataKey::RepoOwner).ok().flatten()?,
        repo.get_metadata(MetadataKey::RepoName).ok().flatten()?,
        repo.get_current_branch_name().ok().flatten()?,
    ))
}

/// The server's approval state for a branch; `None` when it can't say (not
/// pushed, an older server, offline).
pub async fn fetch_approvals(
    (remote, owner, repo, branch): (String, String, String, String),
) -> Option<Value> {
    let url = format!(
        "{}/api/{}/{}/branches/{}/approvals",
        remote.trim_end_matches('/'),
        urlencoding::encode(&owner),
        urlencoding::encode(&repo),
        super::branch_api_segment(&branch),
    );
    let mut request = crate::http::api_client()
        .get(url)
        .timeout(crate::http::read_only_probe_timeout());
    if let Some(token) = super::credentials::effective_token(&remote) {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

impl ImpactJson {
    /// Fold in the server's approval state, or say it's unknown.
    pub fn with_approvals(mut self, approvals: Option<Value>) -> Self {
        let Some(approvals) = approvals else {
            self.caveats.push(
                "approval state is unknown here; the branch may not be pushed, or the server \
                 didn't answer"
                    .to_string(),
            );
            return self;
        };
        self.approvals_enforced = approvals["enforced"].as_bool() == Some(true);
        self.missing_approvals = super::approve::parse_missing(&approvals)
            .into_iter()
            .map(|m| m.project)
            .collect();
        self.approvals = approvals["projects"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        self
    }
}

/// A cheap gate before reading the project index on hot paths: a root
/// `OAK_PROJECT` (a company repo) or a view checkout.
pub fn may_have_projects(repo: &dyn Repository, work_tree: &Path) -> bool {
    work_tree.join(PROJECT_FILE).is_file() || crate::http::view_of_checkout(repo).is_some()
}

/// Every path the current branch changes: its committed contribution since
/// the fork point plus uncommitted work.
pub fn branch_changed_paths(repo: &dyn Repository, work_tree: &Path) -> Result<Vec<String>> {
    let (changes, head, branch_name) =
        crate::commands::commit::compute_changes_for_status(repo, work_tree)?;
    let mut paths: BTreeSet<String> = changes.into_iter().map(|c| c.path).collect();
    if let (Some(branch_name), Some(head)) = (branch_name.as_deref(), head.as_ref()) {
        if let Some(branch) = repo.get_branch(branch_name)? {
            let against = branch
                .parent_branch
                .as_deref()
                .unwrap_or(oak_core::DEFAULT_BRANCH);
            if branch_name != against {
                let (_, base_root, _) =
                    crate::commands::review::branch_fork_base_root(repo, branch_name, against)?;
                let head_root = crate::commands::review::root_for_head(repo, Some(head))?;
                for change in crate::commands::review::diff_roots_with_content_renames(
                    repo, &base_root, &head_root,
                )? {
                    if let Some(old) = change.old_path {
                        paths.insert(old);
                    }
                    paths.insert(change.path);
                }
            }
        }
    }
    Ok(paths.into_iter().collect())
}

fn impact_command(rt: &tokio::runtime::Runtime, work_tree: &Path, json: bool) -> Result<()> {
    let ctx = crate::resolve::resolve(work_tree)?;
    let repo = SqliteRepository::open(&ctx.db_path()?)?;
    let paths = branch_changed_paths(&repo, &ctx.work_tree)?;
    let server = server_index_source(&repo).map(|source| rt.block_on(server_index_for(source)));
    let approvals = approvals_source(&repo).and_then(|source| rt.block_on(fetch_approvals(source)));
    let impact = impact_for(server, &repo, &paths).map(|i| i.with_approvals(approvals));
    if json {
        #[derive(Serialize)]
        struct ImpactCommandJson {
            schema_version: u32,
            changed_paths: usize,
            /// `null` when the repo has no projects.
            impact: Option<ImpactJson>,
            recommended_next_commands: Vec<String>,
        }
        return output::print_json(&ImpactCommandJson {
            schema_version: SCHEMA_VERSION,
            changed_paths: paths.len(),
            recommended_next_commands: impact
                .as_ref()
                .map(|i| {
                    i.dependents_affected
                        .iter()
                        .map(|d| {
                            format!("oak project show {} --json", output::shell_quote(&d.name))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            impact,
        });
    }
    let Some(impact) = impact else {
        output::print_line("This repo has no projects (no OAK_PROJECT files).");
        return Ok(());
    };
    if impact.touched_projects.is_empty() {
        output::print_line("The branch touches no project.");
    }
    for t in &impact.touched_projects {
        output::print_line(&format!(
            "touches {:<20} {} file(s)  maintainers: {}",
            t.name,
            t.changed_files,
            t.maintainers.join(", ")
        ));
    }
    for d in &impact.dependents_affected {
        output::print_line(&format!(
            "  depended on by {:<14} (via {})  maintainers: {}",
            d.name,
            d.depends_on.join(", "),
            d.maintainers.join(", ")
        ));
    }
    if !impact.missing_approvals.is_empty() {
        output::print_line(&format!(
            "still needs approval: {}",
            impact.missing_approvals.join(", ")
        ));
    }
    for c in &impact.caveats {
        output::warning(c);
    }
    Ok(())
}

#[cfg(test)]
mod impact_tests {
    use super::*;

    fn index(records: Vec<Value>) -> Index {
        Index {
            source: "remote",
            remote: None,
            head: None,
            org: None,
            repo: None,
            records,
            hidden_projects: 0,
            unavailable: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn record(name: &str, path: &str, dependents: &[&str]) -> Value {
        serde_json::json!({
            "name": name,
            "path": path,
            "maintainers": [format!("@{name}-team")],
            "dependents": dependents,
            "workflows": [format!("{path}/.oak/workflows/ci.yml")],
        })
    }

    #[test]
    fn impact_names_touched_projects_and_transitive_dependents() {
        // ui <- web <- admin; billing stands alone.
        let index = index(vec![
            record("root", "", &[]),
            record("ui", "libs/ui", &["web"]),
            record("web", "apps/web", &["admin"]),
            record("admin", "apps/web/admin", &[]),
            record("billing", "apps/billing", &[]),
        ]);
        let impact = impact_over(
            &index,
            [
                "libs/ui/button.tsx",
                "libs/ui/theme.css",
                "apps/billing/lib.rs",
            ],
        )
        .unwrap();
        let touched: Vec<(&str, usize)> = impact
            .touched_projects
            .iter()
            .map(|t| (t.name.as_str(), t.changed_files))
            .collect();
        assert_eq!(touched, [("billing", 1), ("ui", 2)]);
        let dependents: Vec<(&str, Vec<String>)> = impact
            .dependents_affected
            .iter()
            .map(|d| (d.name.as_str(), d.depends_on.clone()))
            .collect();
        assert_eq!(
            dependents,
            [
                ("admin", vec!["ui".to_string()]),
                ("web", vec!["ui".to_string()])
            ]
        );
        assert_eq!(impact.dependents_affected[1].maintainers, ["@web-team"]);
        assert_eq!(
            impact.workflows,
            [
                "apps/billing/.oak/workflows/ci.yml",
                "libs/ui/.oak/workflows/ci.yml"
            ]
        );
        assert!(!impact.approvals_enforced && impact.missing_approvals.is_empty());
        let impact = impact.with_approvals(Some(serde_json::json!({
            "enforced": true,
            "projects": [{"project": "ui", "approved": false}],
            "missing_approvals": [{"project": "ui", "path": "libs/ui", "maintainers": ["@a"]}],
        })));
        assert!(impact.approvals_enforced);
        assert_eq!(impact.missing_approvals, ["ui"]);
        assert_eq!(impact.approvals.len(), 1);
        let unknown = impact.clone().with_approvals(None);
        assert!(unknown.caveats.iter().any(|c| c.contains("unknown")));

        // Touching a dependent too: it's touched, not "affected".
        let impact = impact_over(&index, ["libs/ui/a", "apps/web/b"]).unwrap();
        let dependents: Vec<&str> = impact
            .dependents_affected
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(dependents, ["admin"]);

        // Root-owned paths touch the root project; a repo without projects
        // has no impact block at all.
        let impact = impact_over(&index, ["README.md"]).unwrap();
        assert_eq!(impact.touched_projects[0].name, "root");
        assert!(impact_over(&super::impact_tests::index(Vec::new()), ["x"]).is_none());
    }
}
