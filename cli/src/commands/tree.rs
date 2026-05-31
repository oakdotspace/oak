use std::collections::HashMap;
use std::path::Path;

use oak_core::Repository;
use oak_core::{Branch, BranchStatus, Result};

use crate::output;

/// Print all branches as a tree based on their parent_branch links.
pub fn run(path: &Path) -> Result<()> {
    let ctx = crate::resolve::resolve(path)?;
    let repo = ctx.open()?;

    let mut branches = repo.list_branches()?;
    let current = repo.get_current_branch_name()?;

    if branches.is_empty() {
        output::info("No branches");
        return Ok(());
    }

    // Sort siblings alphabetically for stable output.
    branches.sort_by(|a, b| a.name.cmp(&b.name));

    // Set of all branch names so we can detect orphans (parent not in the set).
    let names: std::collections::HashSet<&str> = branches.iter().map(|b| b.name.as_str()).collect();

    // parent_name -> children. None = root.
    let mut children: HashMap<Option<String>, Vec<&Branch>> = HashMap::new();
    for br in &branches {
        let key = match &br.parent_branch {
            Some(p) if names.contains(p.as_str()) => Some(p.clone()),
            _ => None,
        };
        children.entry(key).or_default().push(br);
    }

    let roots: Vec<&Branch> = children.remove(&None).unwrap_or_default();

    for (i, root) in roots.iter().enumerate() {
        let last = i == roots.len() - 1;
        print_node(&*repo, root, "", last, true, current.as_deref(), &children);
    }

    Ok(())
}

fn print_node(
    repo: &dyn Repository,
    br: &Branch,
    prefix: &str,
    is_last: bool,
    is_root: bool,
    current: Option<&str>,
    children: &HashMap<Option<String>, Vec<&Branch>>,
) {
    let connector = if is_root {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };

    output::print_line(&format!(
        "{}{}{}",
        prefix,
        connector,
        format_branch(repo, br, current)
    ));

    let kids = children.get(&Some(br.name.clone()));
    if let Some(kids) = kids {
        let child_prefix = if is_root {
            String::new()
        } else if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };
        for (i, kid) in kids.iter().enumerate() {
            let last = i == kids.len() - 1;
            print_node(repo, kid, &child_prefix, last, false, current, children);
        }
    }
}

fn format_branch(repo: &dyn Repository, br: &Branch, current: Option<&str>) -> String {
    let is_current = current == Some(br.name.as_str());

    let name = if is_current {
        format!(
            "{}{}* {}{}",
            output::colors::GREEN,
            output::colors::BOLD,
            br.name,
            output::colors::RESET,
        )
    } else {
        br.name.clone()
    };

    let status_color = match br.status {
        BranchStatus::Open => output::colors::GREEN,
        BranchStatus::Closed => output::colors::DIM,
    };
    let status = format!("{}{}{}", status_color, br.status, output::colors::RESET);

    let mut line = format!("{name} [{status}]");

    if let Ok(Some(head)) = repo.get_branch_head(&br.name) {
        line.push_str(&format!(
            " {}[head: {}]{}",
            output::colors::CYAN,
            head.short(),
            output::colors::RESET,
        ));
    }

    if let Some(ref desc) = br.description {
        line.push_str(&format!(" - {desc}"));
    }

    line
}
