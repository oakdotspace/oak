// Pure helpers over CLI output. No `vscode` import, so unit tests can run
// under plain Node.

import type { AgentStateJson, LogEntry, StatusChange } from './types';

const LETTERS: Record<string, string> = {
  added: 'A',
  modified: 'M',
  deleted: 'D',
  renamed: 'R',
  conflicted: '!',
  copied: 'C',
  typechange: 'T',
};

const LABELS: Record<string, string> = {
  added: 'Added',
  modified: 'Modified',
  deleted: 'Deleted',
  renamed: 'Renamed',
  conflicted: 'Conflict',
  copied: 'Copied',
  typechange: 'Type Changed',
};

export function statusLetter(status: string): string {
  return LETTERS[status] ?? status.charAt(0).toUpperCase();
}

export function statusLabel(status: string): string {
  return LABELS[status] ?? status.charAt(0).toUpperCase() + status.slice(1);
}

/** Sort by path and drop malformed rows. */
export function normalizeStatusChanges(changes: unknown): StatusChange[] {
  if (!Array.isArray(changes)) {
    return [];
  }
  return changes
    .filter((c): c is StatusChange => !!c && typeof c.path === 'string' && typeof c.status === 'string')
    .slice()
    .sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
}

/** `oak log --json` is an array today; accept an object wrapper too. */
export function normalizeLog(raw: unknown): LogEntry[] {
  const list = Array.isArray(raw)
    ? raw
    : raw && typeof raw === 'object' && Array.isArray((raw as { commits?: unknown }).commits)
      ? (raw as { commits: unknown[] }).commits
      : [];
  return list.filter((e): e is LogEntry => !!e && typeof (e as LogEntry).hash === 'string');
}

/** Human summary for a commit row: the branch description or a generated subject. */
export function commitSubject(entry: LogEntry): string {
  const text = (entry.description_or_subject ?? '').trim();
  const first = text.split(/\r?\n/)[0];
  return first || entry.hash.slice(0, 12);
}

/** "3 minutes ago"-style relative time. */
export function relativeTime(iso: string, now: number = Date.now()): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) {
    return iso;
  }
  const seconds = Math.round((now - then) / 1000);
  if (seconds < 0) {
    return 'just now';
  }
  const units: [number, string][] = [
    [60 * 60 * 24 * 365, 'year'],
    [60 * 60 * 24 * 30, 'month'],
    [60 * 60 * 24 * 7, 'week'],
    [60 * 60 * 24, 'day'],
    [60 * 60, 'hour'],
    [60, 'minute'],
  ];
  for (const [size, name] of units) {
    if (seconds >= size) {
      const n = Math.floor(seconds / size);
      return `${n} ${name}${n === 1 ? '' : 's'} ago`;
    }
  }
  return 'just now';
}

/** Valid Oak branch names: conservative check matching what the server accepts. */
export function validateBranchName(name: string): string | undefined {
  const trimmed = name.trim();
  if (!trimmed) {
    return 'Branch name cannot be empty';
  }
  if (/\s/.test(trimmed)) {
    return 'Branch name cannot contain whitespace';
  }
  if (/[~^:?*[\\]|\.\.|@\{|\/\/|^\/|\/$|^-|\.$|\.lock$/.test(trimmed)) {
    return 'Invalid branch name';
  }
  return undefined;
}

/** Accept "org/repo" only. */
export function validateRepoSpec(spec: string): string | undefined {
  return /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(spec.trim()) ? undefined : 'Expected ORG/REPO';
}

export interface PushState {
  needsPush: boolean;
  unpushed: number;
  /** The server has seen this branch at some point. */
  published: boolean;
}

/**
 * Push state for `branch` from `oak agent state --json`; undefined when the
 * document is missing or describes a different branch (e.g. mid-switch).
 */
export function pushStateFrom(state: AgentStateJson | undefined, branch: string | null | undefined): PushState | undefined {
  if (!state || !branch || state.branch !== branch) {
    return undefined;
  }
  return {
    needsPush: !!state.needs_push,
    unpushed: state.unpushed_commit_count ?? 0,
    published: !!state.current_branch_pushed_head,
  };
}
