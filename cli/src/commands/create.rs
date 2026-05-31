use std::fs;
use std::path::Path;

use oak_core::{Branch, MetadataKey, OakError, Result};
use oak_core::{Repository, SqliteRepository};

use crate::output;

const DEFAULT_REMOTE: &str = "https://oakvcs.com";
const LOCAL_REMOTE: &str = "http://localhost:3000";

/// Create a new Oak repository interactively
pub fn run(path: &Path, name: Option<String>, local: bool, api_key: Option<String>) -> Result<()> {
    // Prompt for name if not provided
    let name = match name {
        Some(n) => n,
        None => {
            let input: String = dialoguer::Input::new()
                .with_prompt("What do you want to call this project?")
                .interact_text()
                .map_err(|e| OakError::Io(std::io::Error::other(e)))?;
            input
        }
    };

    // Validate name
    if name.is_empty() {
        return Err(OakError::Server("Project name cannot be empty".to_string()));
    }
    if name.contains(' ') || name.contains('/') || name.contains('\\') {
        return Err(OakError::Server(
            "Project name cannot contain spaces or path separators".to_string(),
        ));
    }

    let project_dir = path.join(&name);

    // Check that directory does not already exist
    if project_dir.exists() {
        return Err(OakError::Server(format!(
            "Directory '{}' already exists",
            project_dir.display()
        )));
    }

    let default_remote = std::env::var("OAK_REMOTE");
    let remote_url: &str = if local {
        LOCAL_REMOTE
    } else {
        default_remote.as_deref().unwrap_or(DEFAULT_REMOTE)
    };

    // Server-side creation requires an owner under the new `/:owner/:repo`
    // URL scheme, which `oak create` doesn't know. Let first push guide the
    // user through picking an organization and creating the remote repo.
    output::info("Local repository initialized. Run `oak push` to create it on the server.");

    // Create project directory
    fs::create_dir_all(&project_dir)?;

    // Create .oak directory
    let oak_dir = project_dir.join(".oak");
    fs::create_dir_all(&oak_dir)?;

    // Initialize SQLite database
    let db_path = oak_dir.join("oak.db");
    let repo = SqliteRepository::open(&db_path)?;

    // Set metadata
    repo.set_remote_url(remote_url)?;
    repo.set_metadata(MetadataKey::RepoName, &name)?;

    // Store API key if provided
    if let Some(ref key) = api_key {
        repo.set_metadata(MetadataKey::ApiKey, key)?;
    }

    // `main` only exists on the server. Locally users always work on a
    // personal/feature branch parented onto main — same model as `oak init`.
    let branch_name = super::init::default_local_branch_name();
    let default_branch = Branch::new(branch_name.clone(), None, Some("main".to_string()));
    repo.store_branch(&default_branch)?;
    repo.set_current_branch(&branch_name)?;

    output::success(&format!(
        "Created project '{}' at {}",
        name,
        project_dir.display()
    ));
    output::info(&format!(
        "Working on branch '{branch_name}' (parented onto main). Use 'oak commit' to save your changes."
    ));

    Ok(())
}
