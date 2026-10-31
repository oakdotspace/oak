//! oak-cli's integration tests, compiled as ONE test binary.
//!
//! Each file here used to be its own `tests/*.rs` target, and every target
//! statically linked oak-cli and oak-core: ~80 links of the same code and ~2 GB
//! of near-identical executables per debug build. One binary links once.
//!
//! Isolation is unchanged under nextest (`make ci` and CI): every test still
//! runs in its own process, which the mount tests rely on (they share the
//! process-global OAK_MOUNTS_ROOT). Under plain `cargo test` the files now
//! share one process, so process-global state (env vars, statics) set by
//! one file is visible to the others.
//!
//! Add a new test file by dropping it in this directory and listing it below.

mod agent_json;
mod blob_fetch;
mod branch_assessment_robustness;
mod branch_train;
mod case_renames;
mod change_capture;
mod change_capture_council;
mod change_capture_independent;
mod change_capture_release_gate;
mod change_export;
mod ci;
mod ci_approve;
mod ci_cancel_independent;
mod ci_deploy;
mod ci_observation;
mod ci_wait_current;
mod clean_guards;
mod clone_receipt;
mod clone_seed;
mod close_many;
mod close_many_adversarial;
mod credential_origin_scope;
mod desc_sync;
mod description_ack_fence;
mod description_race;
mod diff_evidence;
mod diff_patch_fidelity;
mod feedback_links;
mod file_inspect;
mod findings;
mod finish_description_sync;
mod git_backend;
mod help_surface;
mod hooks;
mod integration;
mod integrity_clone;
mod integrity_evidence;
mod local_inspection;
mod materialize;
mod merge_json_framing;
mod metadata_sync_outcomes;
mod mount_core;
mod mount_lifecycle;
mod mount_remote;
mod mount_ungated;
mod piped_output;
mod pull;
mod pull_local_commit_preservation;
mod pull_receipts;
mod pull_tree_diff;
mod push;
mod qa1_head_only_merge_base;
mod qa_l1_findings;
mod qa_l1_round2;
mod qa_l1_round3;
mod qa_l6_bounds;
mod qa_l7_probe;
mod receipts_identity;
mod redirect;
mod released_v01021_compat;
mod remote_analysis_snapshot;
mod remote_diff_hydration;
mod repo_credential_precedence;
mod repo_visibility;
mod restore_head;
mod review_base_hydration;
mod review_cold_content;
mod review_evidence_ergonomics;
mod review_missing_ancestry;
mod serve_workflows;
mod shallow_parent_refresh;
mod space;
mod space_inventory_verify;
mod sparse;
mod sparse_restricted;
mod split;
mod subdirectory;
mod tree_merge;
mod worktree_moves;
mod zero_byte_blobs;
