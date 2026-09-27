//! `oak logout` — drop a stored server credential.
//!
//! Credentials live in `~/.oak/credentials` as an array keyed by server
//! URL, so logging out removes only the entry for `--remote` and leaves
//! any other servers' logins intact. Logging out of a server you aren't
//! logged in to is a no-op, not an error.

use oak_core::Result;

use crate::output;

use super::credentials::remove_credential;

pub fn run(remote: &str) -> Result<()> {
    let remote = remote.trim_end_matches('/').to_string();

    if remove_credential(&remote)? {
        output::success(&format!("Logged out of {remote}"));
    } else {
        output::info(&format!("Not logged in to {remote}"));
    }

    Ok(())
}
