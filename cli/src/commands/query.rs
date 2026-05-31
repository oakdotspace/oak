use std::path::Path;
use std::process::Command;

use oak_core::OakError;

pub fn run(path: &Path) -> Result<(), OakError> {
    let ctx = crate::resolve::resolve(path)?;
    let db_path = ctx.db_path()?;

    if !db_path.exists() {
        return Err(OakError::RepoNotFound);
    }

    eprintln!(
        "\x1b[33mwarning:\x1b[0m opening repo db in read-write mode. \
         arbitrary writes can corrupt this repository — only proceed if you know what you're doing."
    );

    let status = Command::new("sqlite3").arg(&db_path).status()?;

    if !status.success() {
        return Err(OakError::Database(format!(
            "sqlite3 exited with status {status}"
        )));
    }

    Ok(())
}
