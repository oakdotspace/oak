//! Command-aware construction for agent-facing recommended action metadata.
//!
//! Callers pass the concrete command and the facts they know about the work
//! context. This module owns the action contract so `agent state`, branch
//! review, and branch triage do not reimplement mutability, network, freshness,
//! confidence, or risk-note rules.

use crate::output::{
    AgentRecommendedActionJson, AgentRecommendedActionKindJson as ActionKind, ProgressStateJson,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActionContext {
    Checkout,
    Mount,
    BranchReview { remote: bool },
    BranchTriage { remote: bool },
}

impl AgentActionContext {
    fn is_checkout(self) -> bool {
        matches!(self, Self::Checkout)
    }

    fn is_mount(self) -> bool {
        matches!(self, Self::Mount)
    }

    fn remote_branch_mode(self) -> bool {
        matches!(
            self,
            Self::BranchReview { remote: true } | Self::BranchTriage { remote: true }
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AgentActionState {
    pub dirty: bool,
    pub needs_pull: bool,
    pub needs_push: bool,
    pub unpushed_commit_count: usize,
    pub progress_in_progress: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RemoteFreshnessInput<'a> {
    pub remote_configured: bool,
    pub refresh_requested: bool,
    pub refresh_supported: bool,
    pub refresh_errors: &'a [String],
    pub remote_parent_fetched_at: Option<&'a str>,
    pub current_branch_push_checked: bool,
}

#[derive(Debug)]
pub struct AgentActionDetailInput<'a> {
    pub context: AgentActionContext,
    pub command: String,
    pub kind: Option<ActionKind>,
    pub state: AgentActionState,
    pub remote: RemoteFreshnessInput<'a>,
    pub blocking_reason: Option<String>,
    pub risk_hints: Vec<String>,
    pub confidence_hint: Option<&'a str>,
}

pub fn build(input: AgentActionDetailInput<'_>) -> AgentRecommendedActionJson {
    let remote_freshness = remote_freshness(input.remote);
    let kind = if input.state.progress_in_progress {
        ActionKind::ResolveConflict
    } else if noop_candidate(
        &input.command,
        &input.state,
        input.blocking_reason.as_deref(),
        &remote_freshness,
    ) {
        ActionKind::Noop
    } else {
        input
            .kind
            .unwrap_or_else(|| kind_for_command(&input.command))
    };
    let mutates = mutates(&input.command, kind);
    let needs_network = needs_network(&input.command, kind, input.context, input.remote);
    let confidence = confidence(
        input.confidence_hint,
        needs_network,
        input.context,
        input.remote,
        &remote_freshness,
    );
    let risk_notes = risk_notes(&input, kind);

    AgentRecommendedActionJson {
        kind,
        command: input.command,
        mutates,
        needs_network,
        confidence,
        remote_freshness,
        blocking_reason: input.blocking_reason,
        risk_notes,
    }
}

pub fn remote_freshness(input: RemoteFreshnessInput<'_>) -> String {
    if !input.remote_configured {
        "not_configured".to_string()
    } else if !input.refresh_supported {
        "unsupported".to_string()
    } else if input.refresh_requested && input.refresh_errors.is_empty() {
        "fresh".to_string()
    } else if input.refresh_requested {
        "degraded".to_string()
    } else if input.current_branch_push_checked {
        "fresh".to_string()
    } else if input.remote_parent_fetched_at.is_some() {
        "cached".to_string()
    } else {
        "unknown".to_string()
    }
}

fn kind_for_command(command: &str) -> ActionKind {
    let command = command.trim();
    match command {
        "oak commit" => ActionKind::Commit,
        "oak commit --push" => ActionKind::CommitPush,
        "oak push" => ActionKind::Push,
        _ if command.starts_with("oak push ") => ActionKind::Push,
        "oak pull" | "oak pull --continue" | "oak pull --abort" => ActionKind::Pull,
        "oak merge" | "oak merge --continue" | "oak merge --abort" => ActionKind::Merge,
        "oak status --json" | "oak agent state --json" | "oak status --porcelain" => {
            ActionKind::Inspect
        }
        _ if command.starts_with("oak merge ") => ActionKind::Merge,
        _ if command.starts_with("oak switch ") => ActionKind::ResolveConflict,
        _ if command.starts_with("oak finish ") => ActionKind::Finish,
        _ if command.starts_with("oak close ") => ActionKind::CloseBranch,
        _ if command.starts_with("oak branch review ") => ActionKind::ReviewBranch,
        _ => ActionKind::Inspect,
    }
}

fn mutates(command: &str, kind: ActionKind) -> bool {
    let command = command.trim();
    if matches!(kind, ActionKind::Noop) {
        return false;
    }
    if command.starts_with("oak branch review ")
        || command.starts_with("oak branch diff ")
        || command.starts_with("oak branch triage ")
        || command.starts_with("oak status ")
        || command.starts_with("oak agent state ")
    {
        return false;
    }
    !matches!(kind, ActionKind::Inspect | ActionKind::ReviewBranch)
}

fn needs_network(
    command: &str,
    kind: ActionKind,
    context: AgentActionContext,
    remote: RemoteFreshnessInput<'_>,
) -> bool {
    let command = command.trim();
    if command.starts_with("oak branch review ") {
        return context.remote_branch_mode() || has_flag(command, "--remote");
    }
    if command.starts_with("oak close ") {
        return context.remote_branch_mode()
            || has_flag(command, "--remote")
            || remote.remote_configured;
    }
    if command.starts_with("oak switch ") {
        return false;
    }
    if command.starts_with("oak login ") {
        return true;
    }
    matches!(
        kind,
        ActionKind::CommitPush
            | ActionKind::Push
            | ActionKind::Finish
            | ActionKind::Pull
            | ActionKind::Merge
    )
}

fn has_flag(command: &str, flag: &str) -> bool {
    command.split_whitespace().any(|part| part == flag)
}

fn confidence(
    hint: Option<&str>,
    needs_network: bool,
    context: AgentActionContext,
    remote: RemoteFreshnessInput<'_>,
    remote_freshness: &str,
) -> String {
    if !remote.refresh_errors.is_empty() {
        return "low".to_string();
    }
    if let Some(hint) = hint {
        if needs_network && !matches!(remote_freshness, "fresh") && hint == "high" {
            return "medium".to_string();
        }
        return hint.to_string();
    }
    if needs_network
        && context.is_checkout()
        && !remote.refresh_requested
        && !matches!(remote_freshness, "fresh")
    {
        "medium".to_string()
    } else {
        "high".to_string()
    }
}

fn noop_candidate(
    command: &str,
    state: &AgentActionState,
    blocking_reason: Option<&str>,
    remote_freshness: &str,
) -> bool {
    !state.dirty
        && !state.needs_pull
        && !state.needs_push
        && state.unpushed_commit_count == 0
        && blocking_reason.is_none()
        && remote_freshness == "fresh"
        && matches!(
            command,
            "oak status --json" | "oak agent state --json" | "oak status --porcelain"
        )
}

fn risk_notes(input: &AgentActionDetailInput<'_>, kind: ActionKind) -> Vec<String> {
    let mut notes = Vec::new();
    if input.state.progress_in_progress {
        notes.push("operation_in_progress".to_string());
    }
    if !input.remote.remote_configured {
        notes.push("no_remote_configured".to_string());
    }
    if !input.remote.refresh_errors.is_empty() {
        notes.push("remote_refresh_degraded".to_string());
    }
    if kind == ActionKind::Commit {
        notes.push("commit_is_local_checkpoint_only".to_string());
    }
    if kind == ActionKind::CommitPush {
        notes.push("commit_push_contacts_remote".to_string());
    }
    if kind == ActionKind::Finish && input.state.dirty {
        notes.push("finish_will_commit_and_push".to_string());
    }
    if kind == ActionKind::Pull {
        notes.push("pull_updates_working_tree".to_string());
    }
    if input.state.needs_pull && kind != ActionKind::Pull {
        notes.push("pull_required_after_checkpoint".to_string());
    }
    if (input.state.needs_push || input.state.unpushed_commit_count > 0)
        && !matches!(
            kind,
            ActionKind::Push | ActionKind::Finish | ActionKind::CommitPush
        )
    {
        notes.push("unpushed_commits_remain".to_string());
    }
    if input.context.is_mount() && matches!(kind, ActionKind::Finish | ActionKind::Push) {
        notes.push("mount_state_will_change".to_string());
    }
    notes.extend(input.risk_hints.iter().cloned());
    notes
}

pub fn from_progress_command(
    context: AgentActionContext,
    recommended_next_commands: &[String],
    progress_state: &ProgressStateJson,
    state: AgentActionState,
    remote: RemoteFreshnessInput<'_>,
    blocking_reason: Option<String>,
) -> AgentRecommendedActionJson {
    let mut command = recommended_next_commands
        .first()
        .cloned()
        .unwrap_or_else(|| "oak status --json".to_string());
    if !remote.remote_configured && command == "oak push" {
        command = "oak push --repo <org>/<repo>".to_string();
    }
    build(AgentActionDetailInput {
        context,
        command,
        kind: progress_state
            .in_progress
            .then_some(ActionKind::ResolveConflict),
        state: AgentActionState {
            progress_in_progress: progress_state.in_progress,
            ..state
        },
        remote,
        blocking_reason,
        risk_hints: Vec::new(),
        confidence_hint: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_remote() -> RemoteFreshnessInput<'static> {
        RemoteFreshnessInput {
            remote_configured: true,
            refresh_requested: true,
            refresh_supported: true,
            refresh_errors: &[],
            remote_parent_fetched_at: Some("2026-01-01T00:00:00Z"),
            current_branch_push_checked: true,
        }
    }

    fn no_remote() -> RemoteFreshnessInput<'static> {
        RemoteFreshnessInput {
            remote_configured: false,
            refresh_requested: false,
            refresh_supported: true,
            refresh_errors: &[],
            remote_parent_fetched_at: None,
            current_branch_push_checked: false,
        }
    }

    fn detail(command: &str, context: AgentActionContext) -> AgentRecommendedActionJson {
        build(AgentActionDetailInput {
            context,
            command: command.to_string(),
            kind: None,
            state: AgentActionState::default(),
            remote: fresh_remote(),
            blocking_reason: None,
            risk_hints: Vec::new(),
            confidence_hint: None,
        })
    }

    #[test]
    fn named_merge_mutates_and_needs_network() {
        let action = detail(
            "oak merge feature-branch",
            AgentActionContext::BranchReview { remote: true },
        );

        assert_eq!(action.kind, ActionKind::Merge);
        assert!(action.mutates);
        assert!(action.needs_network);
    }

    #[test]
    fn switch_mutates_without_network() {
        let action = detail(
            "oak switch feature-branch",
            AgentActionContext::BranchReview { remote: true },
        );

        assert_eq!(action.kind, ActionKind::ResolveConflict);
        assert!(action.mutates);
        assert!(!action.needs_network);
    }

    #[test]
    fn remote_review_is_read_only_but_networked() {
        let action = detail(
            "oak branch review feature --remote --merge-preview --json",
            AgentActionContext::BranchTriage { remote: true },
        );

        assert_eq!(action.kind, ActionKind::ReviewBranch);
        assert!(!action.mutates);
        assert!(action.needs_network);
    }

    #[test]
    fn local_and_remote_close_network_use_depends_on_context() {
        let local = build(AgentActionDetailInput {
            context: AgentActionContext::BranchReview { remote: false },
            command: "oak close local-branch --reason stale --json".to_string(),
            kind: Some(ActionKind::CloseBranch),
            state: AgentActionState::default(),
            remote: no_remote(),
            blocking_reason: None,
            risk_hints: Vec::new(),
            confidence_hint: None,
        });
        let remote = build(AgentActionDetailInput {
            context: AgentActionContext::BranchReview { remote: true },
            command: "oak close remote-branch --remote --reason stale --json".to_string(),
            kind: Some(ActionKind::CloseBranch),
            state: AgentActionState::default(),
            remote: fresh_remote(),
            blocking_reason: None,
            risk_hints: Vec::new(),
            confidence_hint: None,
        });

        assert!(local.mutates);
        assert!(!local.needs_network);
        assert!(remote.mutates);
        assert!(remote.needs_network);
    }

    #[test]
    fn checkout_review_and_triage_share_merge_command_contract() {
        let contexts = [
            AgentActionContext::Checkout,
            AgentActionContext::BranchReview { remote: true },
            AgentActionContext::BranchTriage { remote: true },
        ];

        for context in contexts {
            let action = build(AgentActionDetailInput {
                context,
                command: "oak merge feature-branch".to_string(),
                kind: Some(ActionKind::Merge),
                state: AgentActionState::default(),
                remote: fresh_remote(),
                blocking_reason: None,
                risk_hints: Vec::new(),
                confidence_hint: Some("medium"),
            });

            assert_eq!(action.kind, ActionKind::Merge);
            assert!(action.mutates);
            assert!(action.needs_network);
            assert_eq!(action.confidence, "medium");
        }
    }

    #[test]
    fn noop_requires_fresh_idle_state() {
        let progress = ProgressStateJson::default();
        let fresh = from_progress_command(
            AgentActionContext::Checkout,
            &["oak status --json".to_string()],
            &progress,
            AgentActionState::default(),
            fresh_remote(),
            None,
        );
        let unknown = from_progress_command(
            AgentActionContext::Checkout,
            &["oak status --json".to_string()],
            &progress,
            AgentActionState::default(),
            RemoteFreshnessInput {
                remote_configured: true,
                refresh_requested: false,
                refresh_supported: true,
                refresh_errors: &[],
                remote_parent_fetched_at: None,
                current_branch_push_checked: false,
            },
            None,
        );

        assert_eq!(fresh.kind, ActionKind::Noop);
        assert_eq!(unknown.kind, ActionKind::Inspect);
    }
}
