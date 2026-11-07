//! Projects — `OAK_PROJECT` files and the index built from them.
//!
//! A **project** is a directory of a company repo that has an `OAK_PROJECT`
//! file at its root. The file names the project, its maintainers, who may
//! read and write it, which other paths it depends on, and optional
//! per-path rules — like a Google `OWNERS` file, it lives next to the code
//! and moves with the directory.
//!
//! ```toml
//! name        = "web"
//! description = "Customer web app"
//! maintainers = ["@group/web-team", "@alice"]
//! writers     = ["@group/eng"]        # default: everyone who can read
//! readers     = "org"                 # "org" | "restricted" | ["@group/x", ...]
//! deps        = ["ui", "/libs/auth"]  # project names, or paths ("/x" or "a/b")
//! self_approve = false
//!
//! [paths]                             # relative to the project; last match wins
//! "config/prod/" = ["@group/sre"]
//! "docs/"        = ["*:read", "@group/docs"]
//! ```
//!
//! Principals are `@user`, `@group/slug`,
//! `*` (everyone who can read the repo), `@public`, and a `:read` suffix for
//! read-only grants inside `[paths]`. Parsing is shared by the CLI (offline
//! project resolution) and the server (enforcement), so it lives here with no
//! I/O. `OAK_PROJECT` files are the only source of path permissions, so
//! problems fail closed: an unparsable
//! principal is dropped (restricting more), and a file that won't parse is
//! reported as an error so the server can lock its subtree.

use crate::sparse::normalize_prefix;
use crate::tree::validate_tree_path;
use crate::view::{ViewSpec, DEFAULT_SPINE_FILES};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The project file's name, at the project's root directory.
pub const PROJECT_FILE: &str = "OAK_PROJECT";

/// Larger files are refused outright.
pub const MAX_PROJECT_FILE_BYTES: usize = 64 * 1024;

/// Who a rule names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
    /// `@alice`
    User(String),
    /// `@group/ops` — a group of the repo's owning org.
    Group(String),
    /// `*` — everyone who can read the repo.
    Everyone,
    /// `@public` — the world.
    Public,
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Principal::User(u) => write!(f, "@{u}"),
            Principal::Group(g) => write!(f, "@group/{g}"),
            Principal::Everyone => f.write_str("*"),
            Principal::Public => f.write_str("@public"),
        }
    }
}

/// A principal in a `[paths]` rule, read-write unless suffixed `:read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub principal: Principal,
    pub read_only: bool,
}

/// Who may read a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readers {
    /// Every member of the org (the default).
    Org,
    /// Only the project's maintainers, writers, and these extra readers.
    Restricted(Vec<Principal>),
}

/// One `[paths]` entry. Patterns are gitignore-shaped and relative to the
/// project root; an empty `grants` list means org owners/admins only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    pub pattern: String,
    pub grants: Vec<Grant>,
}

/// A dependency: another project by name, or a path prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dep {
    Project(String),
    /// Normalized repo-relative prefix.
    Path(String),
}

/// A parsed `OAK_PROJECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    pub name: String,
    pub description: Option<String>,
    pub maintainers: Vec<Principal>,
    /// `None` means everyone who can read may write (merges are still gated
    /// by maintainer approval).
    pub writers: Option<Vec<Principal>>,
    pub readers: Readers,
    pub deps: Vec<Dep>,
    pub self_approve: bool,
    /// In file order — order is load-bearing (last match wins).
    pub paths: Vec<PathRule>,
    /// Repo-relative paths every view includes. Honored only in the root
    /// project's file.
    pub view_always: Vec<String>,
    /// Whether any principal anywhere is `@public` (adding one needs an org
    /// owner/admin).
    pub names_public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The file (or index entry) is unusable.
    Error,
    /// Something was ignored; the rest stands.
    Warning,
}

/// A problem found while parsing a file or building the index, for the
/// settings page and `oak project show`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    /// The project directory the problem is about (`""` for the root).
    pub path: String,
    pub message: String,
}

impl Problem {
    fn error(path: &str, message: impl Into<String>) -> Self {
        Problem {
            severity: Severity::Error,
            path: path.to_string(),
            message: message.into(),
        }
    }

    fn warning(path: &str, message: impl Into<String>) -> Self {
        Problem {
            severity: Severity::Warning,
            path: path.to_string(),
            message: message.into(),
        }
    }
}

const KNOWN_KEYS: [&str; 9] = [
    "name",
    "description",
    "maintainers",
    "writers",
    "readers",
    "deps",
    "self_approve",
    "paths",
    "view_always",
];

/// Is `name` a valid project name? 1–100 characters of `[A-Za-z0-9._-]`,
/// starting and ending with a letter or digit — the same shape repo names
/// take, since the two share one namespace per org.
pub fn is_valid_project_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_alphanumeric();
    !bytes.is_empty()
        && bytes.len() <= 100
        && edge_ok(bytes[0])
        && edge_ok(bytes[bytes.len() - 1])
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Parse one principal token, with an optional `:read` suffix. `None` when
/// the token isn't a principal.
fn parse_grant(token: &str) -> Option<Grant> {
    let (body, read_only) = match token.strip_suffix(":read") {
        Some(body) => (body, true),
        None => (token, false),
    };
    let slug_ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    };
    let principal = match body {
        "*" => Principal::Everyone,
        "@public" => Principal::Public,
        _ => {
            let rest = body.strip_prefix('@')?;
            match rest.strip_prefix("group/") {
                Some(group) if slug_ok(group) => Principal::Group(group.to_string()),
                Some(_) => return None,
                None if slug_ok(rest) => Principal::User(rest.to_string()),
                None => return None,
            }
        }
    };
    Some(Grant {
        principal,
        read_only,
    })
}

/// Parse an `OAK_PROJECT` file located at `dir` (`""` for the repo root;
/// used only to label problems). Returns the file when it is usable, plus
/// every problem found; any [`Severity::Error`] means `None`.
pub fn parse_project_file(dir: &str, bytes: &[u8]) -> (Option<ProjectFile>, Vec<Problem>) {
    let mut problems = Vec::new();
    if bytes.len() > MAX_PROJECT_FILE_BYTES {
        problems.push(Problem::error(
            dir,
            format!("{PROJECT_FILE} is larger than {MAX_PROJECT_FILE_BYTES} bytes"),
        ));
        return (None, problems);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        problems.push(Problem::error(dir, format!("{PROJECT_FILE} is not UTF-8")));
        return (None, problems);
    };
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(e) => {
            problems.push(Problem::error(
                dir,
                format!("{PROJECT_FILE} is not valid TOML: {}", e.message()),
            ));
            return (None, problems);
        }
    };

    for key in table.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            problems.push(Problem::warning(
                dir,
                format!("unknown key {key:?} ignored"),
            ));
        }
    }

    let mut names_public = false;
    let mut principals = |key: &str, problems: &mut Vec<Problem>| -> Option<Vec<Principal>> {
        let value = table.get(key)?;
        let Some(items) = value.as_array() else {
            problems.push(Problem::error(dir, format!("{key} must be a list")));
            return Some(Vec::new());
        };
        let mut out = Vec::new();
        for item in items {
            match item.as_str().and_then(parse_grant) {
                Some(Grant {
                    read_only: true, ..
                }) => problems.push(Problem::warning(
                    dir,
                    format!("{key}: \":read\" only applies inside [paths]; entry dropped"),
                )),
                Some(grant) => {
                    names_public |= grant.principal == Principal::Public;
                    out.push(grant.principal);
                }
                None => problems.push(Problem::warning(
                    dir,
                    format!("{key}: {item} is not a principal; dropped"),
                )),
            }
        }
        Some(out)
    };

    let maintainers = principals("maintainers", &mut problems).unwrap_or_default();
    let writers = principals("writers", &mut problems);
    let readers = match table.get("readers") {
        None => Readers::Org,
        Some(toml::Value::String(s)) if s == "org" => Readers::Org,
        Some(toml::Value::String(s)) if s == "restricted" => Readers::Restricted(Vec::new()),
        Some(toml::Value::Array(_)) => {
            Readers::Restricted(principals("readers", &mut problems).unwrap_or_default())
        }
        Some(other) => {
            problems.push(Problem::error(
                dir,
                format!("readers must be \"org\", \"restricted\", or a list, not {other}"),
            ));
            Readers::Restricted(Vec::new())
        }
    };

    let string = |key: &str, problems: &mut Vec<Problem>| -> Option<String> {
        match table.get(key)? {
            toml::Value::String(s) => Some(s.clone()),
            other => {
                problems.push(Problem::error(
                    dir,
                    format!("{key} must be a string, not {other}"),
                ));
                None
            }
        }
    };
    let name = string("name", &mut problems);
    match &name {
        None if !problems.iter().any(|p| p.message.starts_with("name ")) => {
            problems.push(Problem::error(dir, "name is required"))
        }
        Some(n) if !is_valid_project_name(n) => problems.push(Problem::error(
            dir,
            format!(
                "name {n:?} must be 1-100 letters, digits, '.', '_' or '-', starting and ending with a letter or digit"
            ),
        )),
        _ => {}
    }
    let description = string("description", &mut problems);

    let self_approve = match table.get("self_approve") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(other) => {
            problems.push(Problem::error(
                dir,
                format!("self_approve must be true or false, not {other}"),
            ));
            false
        }
    };

    let mut deps = Vec::new();
    if let Some(value) = table.get("deps") {
        match value.as_array() {
            None => problems.push(Problem::error(dir, "deps must be a list")),
            Some(items) => {
                for item in items {
                    let Some(raw) = item.as_str() else {
                        problems.push(Problem::warning(
                            dir,
                            format!("deps: {item} is not a string; dropped"),
                        ));
                        continue;
                    };
                    match parse_dep(raw) {
                        Ok(dep) => deps.push(dep),
                        Err(message) => {
                            problems.push(Problem::warning(dir, format!("deps: {message}")))
                        }
                    }
                }
            }
        }
    }
    deps.sort();
    deps.dedup();

    let mut paths = Vec::new();
    if let Some(value) = table.get("paths") {
        match value.as_table() {
            None => problems.push(Problem::error(dir, "[paths] must be a table")),
            Some(rules) => {
                for (pattern, grants) in rules {
                    let Some(items) = grants.as_array() else {
                        problems.push(Problem::warning(
                            dir,
                            format!("[paths] {pattern:?} must map to a list; rule dropped"),
                        ));
                        continue;
                    };
                    let mut parsed = Vec::new();
                    for item in items {
                        match item.as_str().and_then(parse_grant) {
                            Some(grant) => {
                                names_public |= grant.principal == Principal::Public;
                                parsed.push(grant);
                            }
                            None => problems.push(Problem::warning(
                                dir,
                                format!("[paths] {pattern:?}: {item} is not a principal; dropped"),
                            )),
                        }
                    }
                    paths.push(PathRule {
                        pattern: pattern.clone(),
                        grants: parsed,
                    });
                }
            }
        }
    }

    let mut view_always = Vec::new();
    if let Some(value) = table.get("view_always") {
        match value.as_array() {
            None => problems.push(Problem::error(dir, "view_always must be a list")),
            Some(items) => {
                if !dir.is_empty() {
                    problems.push(Problem::warning(
                        dir,
                        "view_always is only honored in the repo root's OAK_PROJECT",
                    ));
                }
                for item in items {
                    match item.as_str().map(normalize_prefix) {
                        Some(p) if !p.is_empty() && validate_tree_path(&p).is_ok() => {
                            view_always.push(p)
                        }
                        _ => problems.push(Problem::warning(
                            dir,
                            format!("view_always: {item} is not a path; dropped"),
                        )),
                    }
                }
            }
        }
    }

    if problems.iter().any(|p| p.severity == Severity::Error) {
        return (None, problems);
    }
    let file = ProjectFile {
        name: name.expect("checked above"),
        description,
        maintainers,
        writers,
        readers,
        deps,
        self_approve,
        paths,
        view_always,
        names_public,
    };
    (Some(file), problems)
}

/// A dep containing `/` (or starting with one) is a path; anything else is
/// a project name. So a top-level directory dep is written `"/libs"`.
fn parse_dep(raw: &str) -> Result<Dep, String> {
    let raw = raw.trim();
    if raw.contains('/') {
        let path = normalize_prefix(raw);
        if path.is_empty() {
            return Err(format!("{raw:?} would depend on the whole repo; dropped"));
        }
        validate_tree_path(&path).map_err(|e| format!("{raw:?}: {e}; dropped"))?;
        Ok(Dep::Path(path))
    } else if is_valid_project_name(raw) {
        Ok(Dep::Project(raw.to_string()))
    } else {
        Err(format!(
            "{raw:?} is neither a project name nor a path; dropped"
        ))
    }
}

/// One project in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The project directory, normalized (`""` for the repo root).
    pub path: String,
    pub file: ProjectFile,
}

/// Every project in a tree, with names, ownership, and dependency queries.
/// Build it from the `OAK_PROJECT` files at one commit with
/// [`ProjectIndex::build`].
#[derive(Debug, Clone, Default)]
pub struct ProjectIndex {
    /// Sorted by path.
    projects: Vec<Project>,
    by_name: BTreeMap<String, usize>,
    /// Resolved direct deps per project (same order as `projects`), as
    /// normalized path prefixes.
    deps: Vec<Vec<String>>,
}

fn covers(prefix: &str, path: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || (path.len() > prefix.len()
            && path.as_bytes()[prefix.len()] == b'/'
            && path.starts_with(prefix))
}

impl ProjectIndex {
    /// Build the index from `(directory, parsed file)` pairs. A duplicate
    /// name keeps the project with the lexically first path and reports the
    /// rest as errors; a dep naming no project is dropped with a warning.
    pub fn build<I>(files: I) -> (ProjectIndex, Vec<Problem>)
    where
        I: IntoIterator<Item = (String, ProjectFile)>,
    {
        let mut problems = Vec::new();
        let mut projects: Vec<Project> = files
            .into_iter()
            .map(|(path, file)| Project {
                path: normalize_prefix(&path),
                file,
            })
            .collect();
        projects.sort_by(|a, b| a.path.cmp(&b.path));
        projects.dedup_by(|a, b| a.path == b.path);

        let mut by_name = BTreeMap::new();
        let mut kept = Vec::with_capacity(projects.len());
        for project in projects {
            if let Some(&first) = by_name.get(&project.file.name) {
                let first: &Project = &kept[first];
                problems.push(Problem::error(
                    &project.path,
                    format!(
                        "project name {:?} is already used by {}",
                        project.file.name,
                        display_dir(&first.path)
                    ),
                ));
                continue;
            }
            by_name.insert(project.file.name.clone(), kept.len());
            kept.push(project);
        }

        let deps = kept
            .iter()
            .map(|project| {
                let mut resolved = Vec::new();
                for dep in &project.file.deps {
                    match dep {
                        Dep::Path(p) => resolved.push(p.clone()),
                        Dep::Project(name) => match by_name.get(name) {
                            Some(&i) => resolved.push(kept[i].path.clone()),
                            None => problems.push(Problem::warning(
                                &project.path,
                                format!("deps: no project named {name:?}; dropped"),
                            )),
                        },
                    }
                }
                resolved.sort();
                resolved.dedup();
                resolved
            })
            .collect();

        (
            ProjectIndex {
                projects: kept,
                by_name,
                deps,
            },
            problems,
        )
    }

    /// All projects, sorted by path.
    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn get(&self, name: &str) -> Option<&Project> {
        self.by_name.get(name).map(|&i| &self.projects[i])
    }

    /// The project at exactly this directory.
    pub fn at_path(&self, dir: &str) -> Option<&Project> {
        let dir = normalize_prefix(dir);
        self.projects
            .binary_search_by(|p| p.path.as_str().cmp(&dir))
            .ok()
            .map(|i| &self.projects[i])
    }

    /// The project that owns `path`: the deepest one whose directory covers
    /// it. `None` when no project (not even a root one) does.
    pub fn owner_of(&self, path: &str) -> Option<&Project> {
        let path = normalize_prefix(path);
        self.projects
            .iter()
            .filter(|p| covers(&p.path, &path))
            .max_by_key(|p| p.path.len())
    }

    /// The root project's file, if the repo root has an `OAK_PROJECT`.
    pub fn root(&self) -> Option<&Project> {
        self.at_path("")
    }

    /// The direct deps of `name`, resolved to path prefixes.
    pub fn direct_deps(&self, name: &str) -> Option<&[String]> {
        self.by_name.get(name).map(|&i| self.deps[i].as_slice())
    }

    /// The path prefixes `names` need: their own directories plus every
    /// transitive dep. A dep that is exactly a project's directory pulls in
    /// that project's deps too; a dep path inside a project doesn't (it's a
    /// slice, not the project). Cycles are fine. Unknown names are skipped.
    pub fn dep_closure<S: AsRef<str>>(&self, names: &[S]) -> Vec<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = names
            .iter()
            .filter_map(|n| self.get(n.as_ref()))
            .map(|p| p.path.clone())
            .collect();
        while let Some(path) = queue.pop_front() {
            if !seen.insert(path.clone()) {
                continue;
            }
            if let Some(project) = self.at_path(&path) {
                let i = self.by_name[&project.file.name];
                queue.extend(self.deps[i].iter().cloned());
            }
        }
        seen.into_iter().collect()
    }

    /// Names of every project that depends on `name`, directly or
    /// transitively, sorted. A project depends on `name` when one of its
    /// resolved deps covers or sits inside `name`'s directory.
    pub fn dependents(&self, name: &str) -> Vec<String> {
        let Some(target) = self.get(name) else {
            return Vec::new();
        };
        let mut found: BTreeSet<String> = BTreeSet::new();
        let mut frontier = vec![target.path.clone()];
        while let Some(dir) = frontier.pop() {
            for (i, project) in self.projects.iter().enumerate() {
                if found.contains(&project.file.name) || project.file.name == name {
                    continue;
                }
                if self.deps[i]
                    .iter()
                    .any(|d| covers(d, &dir) || covers(&dir, d))
                {
                    found.insert(project.file.name.clone());
                    frontier.push(project.path.clone());
                }
            }
        }
        found.into_iter().collect()
    }

    /// The projects owning any of `paths`, by name, sorted. Paths no project
    /// owns are ignored.
    pub fn touched<I, S>(&self, paths: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let names: BTreeSet<String> = paths
            .into_iter()
            .filter_map(|p| self.owner_of(p.as_ref()).map(|o| o.file.name.clone()))
            .collect();
        names.into_iter().collect()
    }

    /// The view for working on `names`: their directories (plus transitive
    /// deps when `with_deps`), the root project's `view_always` paths, and
    /// the default spine files. Nested projects inside an included directory
    /// come along — a project is its whole directory. The server narrows the
    /// result further to what the caller may read.
    pub fn view_for<S: AsRef<str>>(
        &self,
        names: &[S],
        with_deps: bool,
    ) -> Result<ViewSpec, String> {
        let mut include: Vec<String> = Vec::new();
        for name in names {
            let Some(project) = self.get(name.as_ref()) else {
                return Err(format!("no project named {:?}", name.as_ref()));
            };
            include.push(project.path.clone());
        }
        if with_deps {
            include = self.dep_closure(names);
        }
        if let Some(root) = self.root() {
            include.extend(root.file.view_always.iter().cloned());
        }
        ViewSpec::new(include, [] as [&str; 0], DEFAULT_SPINE_FILES).map_err(|e| e.to_string())
    }
}

fn display_dir(path: &str) -> String {
    if path.is_empty() {
        "the repo root".to_string()
    } else {
        format!("{path}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(dir: &str, text: &str) -> ProjectFile {
        let (file, problems) = parse_project_file(dir, text.as_bytes());
        assert!(
            problems.iter().all(|p| p.severity == Severity::Warning),
            "{problems:?}"
        );
        file.unwrap()
    }

    fn errors(text: &str) -> Vec<String> {
        let (file, problems) = parse_project_file("x", text.as_bytes());
        assert!(file.is_none());
        problems
            .into_iter()
            .filter(|p| p.severity == Severity::Error)
            .map(|p| p.message)
            .collect()
    }

    #[test]
    fn parses_the_documented_example() {
        let file = parse(
            "apps/web",
            r#"
name        = "web"
description = "Customer web app"
maintainers = ["@group/web-team", "@alice"]
writers     = ["@group/eng"]
readers     = "org"
deps        = ["ui", "/libs/auth"]

[paths]
"config/prod/" = ["@group/sre"]
"docs/"        = ["*:read", "@group/docs"]
"vault/"       = []
"#,
        );
        assert_eq!(file.name, "web");
        assert_eq!(
            file.maintainers,
            [
                Principal::Group("web-team".into()),
                Principal::User("alice".into())
            ]
        );
        assert_eq!(file.writers, Some(vec![Principal::Group("eng".into())]));
        assert_eq!(file.readers, Readers::Org);
        assert_eq!(
            file.deps,
            [Dep::Project("ui".into()), Dep::Path("libs/auth".into())]
        );
        let patterns: Vec<&str> = file.paths.iter().map(|r| r.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["config/prod/", "docs/", "vault/"],
            "file order kept"
        );
        assert_eq!(
            file.paths[1].grants[0],
            Grant {
                principal: Principal::Everyone,
                read_only: true
            }
        );
        assert!(file.paths[2].grants.is_empty());
        assert!(!file.names_public);
        assert!(!file.self_approve);
    }

    #[test]
    fn path_rule_order_survives_many_keys() {
        let mut text = String::from("name = \"p\"\n[paths]\n");
        let keys: Vec<String> = (0..40).rev().map(|i| format!("dir{i:02}/")).collect();
        for k in &keys {
            text.push_str(&format!("\"{k}\" = [\"*\"]\n"));
        }
        let file = parse("p", &text);
        let got: Vec<&String> = file.paths.iter().map(|r| &r.pattern).collect();
        assert_eq!(got, keys.iter().collect::<Vec<_>>());
    }

    #[test]
    fn readers_forms() {
        assert_eq!(
            parse("a", "name = \"a\"\nreaders = \"restricted\"").readers,
            Readers::Restricted(vec![])
        );
        assert_eq!(
            parse("a", "name = \"a\"\nreaders = [\"@group/sec\"]").readers,
            Readers::Restricted(vec![Principal::Group("sec".into())])
        );
        assert!(!errors("name = \"a\"\nreaders = \"everyone\"").is_empty());
    }

    #[test]
    fn bad_principals_are_dropped_with_warnings() {
        let (file, problems) = parse_project_file(
            "a",
            br#"name = "a"
maintainers = ["@alice", "alice", "@group/", "@bob:read", "@public"]
"#,
        );
        let file = file.unwrap();
        assert_eq!(
            file.maintainers,
            [Principal::User("alice".into()), Principal::Public]
        );
        assert!(file.names_public);
        assert_eq!(problems.len(), 3, "{problems:?}");
    }

    #[test]
    fn errors_fail_the_whole_file() {
        assert!(errors("not = [toml")
            .iter()
            .any(|m| m.contains("not valid TOML")));
        assert!(errors("description = \"x\"").contains(&"name is required".to_string()));
        assert!(errors("name = \"-bad\"")
            .iter()
            .any(|m| m.contains("must be 1-100")));
        assert!(errors("name = 3")
            .iter()
            .any(|m| m.contains("name must be a string")));
        assert!(errors("name = \"a\"\ndeps = \"ui\"").contains(&"deps must be a list".to_string()));
        let (file, problems) = parse_project_file("x", &vec![b'a'; MAX_PROJECT_FILE_BYTES + 1]);
        assert!(file.is_none() && problems[0].message.contains("larger than"));
        let (file, _) = parse_project_file("x", b"\xff\xfe");
        assert!(file.is_none());
    }

    #[test]
    fn unknown_keys_and_misplaced_view_always_warn() {
        let (file, problems) = parse_project_file(
            "apps/web",
            b"name = \"web\"\nowners = []\nview_always = [\"x\"]",
        );
        assert!(file.is_some());
        let messages: Vec<&str> = problems.iter().map(|p| p.message.as_str()).collect();
        assert!(messages
            .iter()
            .any(|m| m.contains("unknown key \"owners\"")));
        assert!(messages
            .iter()
            .any(|m| m.contains("only honored in the repo root")));
    }

    fn index() -> ProjectIndex {
        let files = vec![
            (
                "".to_string(),
                parse(
                    "",
                    "name = \"company\"\nview_always = [\"rust-toolchain.toml\"]",
                ),
            ),
            (
                "apps/web".to_string(),
                parse(
                    "apps/web",
                    "name = \"web\"\ndeps = [\"ui\", \"/libs/auth\"]",
                ),
            ),
            (
                "apps/web/legacy".to_string(),
                parse("apps/web/legacy", "name = \"web-legacy\""),
            ),
            (
                "apps/billing".to_string(),
                parse("apps/billing", "name = \"billing\"\ndeps = [\"auth\"]"),
            ),
            (
                "libs/ui".to_string(),
                parse("libs/ui", "name = \"ui\"\ndeps = [\"tokens\", \"nope\"]"),
            ),
            (
                "libs/tokens".to_string(),
                parse("libs/tokens", "name = \"tokens\"\ndeps = [\"ui\"]"),
            ),
            (
                "libs/auth".to_string(),
                parse("libs/auth", "name = \"auth\""),
            ),
        ];
        let (index, problems) = ProjectIndex::build(files);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].message.contains("no project named \"nope\""));
        index
    }

    #[test]
    fn owner_is_the_deepest_project() {
        let index = index();
        assert_eq!(
            index.owner_of("apps/web/src/a.ts").unwrap().file.name,
            "web"
        );
        assert_eq!(
            index.owner_of("apps/web/legacy/x").unwrap().file.name,
            "web-legacy"
        );
        assert_eq!(index.owner_of("apps/webby/x").unwrap().file.name, "company");
        assert_eq!(index.owner_of("README.md").unwrap().file.name, "company");
        assert_eq!(
            index.touched(["apps/web/a", "libs/ui/b", "libs/ui/c"]),
            ["ui", "web"]
        );
    }

    #[test]
    fn dep_closure_follows_projects_and_tolerates_cycles() {
        let index = index();
        assert_eq!(
            index.dep_closure(&["web"]),
            ["apps/web", "libs/auth", "libs/tokens", "libs/ui"]
        );
        assert_eq!(index.dep_closure(&["tokens"]), ["libs/tokens", "libs/ui"]);
        assert_eq!(index.dependents("auth"), ["billing", "web"]);
        assert_eq!(index.dependents("tokens"), ["ui", "web"]);
        assert!(index.dependents("web").is_empty());
    }

    #[test]
    fn view_for_unions_projects_deps_and_view_always() {
        let index = index();
        let v = index.view_for(&["web", "billing"], true).unwrap();
        assert_eq!(
            v.include(),
            [
                "apps/billing",
                "apps/web",
                "libs/auth",
                "libs/tokens",
                "libs/ui",
                "rust-toolchain.toml"
            ]
        );
        assert_eq!(v.spine_files(), ["AGENTS.md", "CLAUDE.md"]);
        let bare = index.view_for(&["web"], false).unwrap();
        assert_eq!(bare.include(), ["apps/web", "rust-toolchain.toml"]);
        assert!(index.view_for(&["missing"], true).is_err());
    }

    #[test]
    fn duplicate_names_keep_the_first_path() {
        let (index, problems) = ProjectIndex::build(vec![
            ("b".to_string(), parse("b", "name = \"dup\"")),
            ("a".to_string(), parse("a", "name = \"dup\"")),
        ]);
        assert_eq!(index.get("dup").unwrap().path, "a");
        assert_eq!(problems[0].path, "b");
        assert_eq!(problems[0].severity, Severity::Error);
    }
}
