//! oak-core's integration tests, compiled as ONE test binary.
//!
//! Each file here used to be its own `tests/*.rs` target, each statically
//! linking all of oak-core. One binary links once.
//!
//! Isolation is unchanged under nextest (`make ci` and CI): every test still
//! runs in its own process. Under plain `cargo test` the files now share one
//! process, so process-global state set by one file is visible to the others.
//!
//! Add a new test file by dropping it in this directory and listing it below.

mod change_capture_lookup;
mod change_set;
mod pinned_inspection_snapshot;
