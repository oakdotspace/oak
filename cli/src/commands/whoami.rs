//! `oak whoami` — print the logged-in username for a server.
//!
//! Reads the stored credential for `--remote` from `~/.oak/credentials`
//! and prints just the username on stdout (so it's scriptable). Exits
//! non-zero when there's no credential for that server.

use oak_core::{OakError, Result};

use crate::output;

use super::credentials::get_username_for_server;

pub fn run(remote: &str) -> Result<()> {
    let remote = remote.trim_end_matches('/').to_string();

    match get_username_for_server(&remote) {
        Some(username) => {
            output::info(&username);
            Ok(())
        }
        None => Err(OakError::Server(format!(
            "Not logged in to {remote}. Run `oak login` first."
        ))),
    }
}
