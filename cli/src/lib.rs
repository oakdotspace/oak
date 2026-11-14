//! Oak CLI library
//!
//! This module exposes CLI command implementations for use in integration tests.

pub mod agent_action;
pub mod atomic_file;
pub mod commands;
pub mod contract;
pub mod file_permissions;
pub mod http;
pub mod materialize;
#[cfg(test)]
mod no_ui_in_tests;
pub mod output;
pub mod pathutil;
pub mod publication_state;
pub mod remote_head;
pub mod resolve;
pub(crate) mod transfer;
pub mod work_state;
pub mod workdir_lock;
pub mod worktree_update;
