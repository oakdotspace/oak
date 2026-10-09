// Shapes of the `oak ... --json` documents the extension reads. Oak's JSON
// contract is append-only within a schema_version and omits defaulted fields,
// so every field the extension does not strictly need is optional and unknown
// fields are ignored.

export type ChangeStatus = 'added' | 'modified' | 'deleted' | 'renamed' | 'conflicted' | string;

export interface StatusChange {
  path: string;
  status: ChangeStatus;
  /** Present on renames. */
  old_path?: string;
  from?: string;
}

export interface StatusJson {
  schema_version: number;
  branch: string | null;
  branch_description?: string | null;
  parent?: string | null;
  head?: string | null;
  branch_status?: string;
  unmerged_commit_count?: number;
  changes: StatusChange[];
  working_changes?: { base?: string | null; changes: StatusChange[] };
  branch_changes?: { base?: string | null; head?: string | null; changes: StatusChange[] };
  merge_in_progress?: boolean;
  sync_in_progress?: boolean;
  progress_state?: { in_progress: boolean; kind?: string };
  repo_owner?: string | null;
  repo_name?: string | null;
  remote_url?: string | null;
  repository_root?: string;
  web_url?: string | null;
  review_url?: string | null;
}

export interface ConflictStatusJson {
  schema_version: number;
  context?: string;
  in_progress: boolean;
  /** "merge", "sync" (pull), or "mount_pull". */
  kind: string | null;
  conflict_paths: string[];
  recommended_next_commands?: string[];
}

export interface LogEntry {
  hash: string;
  timestamp: string;
  branch?: string | null;
  description_or_subject?: string | null;
  files_changed?: number;
  author?: string | null;
}

export interface BranchEntry {
  name: string;
  head?: string | null;
  description?: string | null;
  status?: string;
  created_at?: string;
  current?: boolean;
}

export interface EndpointDiffFile {
  path: string;
  status: ChangeStatus;
  additions?: number;
  deletions?: number;
  old_path?: string;
}

export interface EndpointDiffJson {
  changed_file_count?: number;
  changed_files: EndpointDiffFile[];
  changed_files_page?: { next_offset?: number | null; total_count?: number };
}

export interface CommitJson {
  committed: boolean;
  pushed?: boolean;
  branch?: string;
  head_before?: string | null;
  head_after?: string | null;
  unpushed_commit_count?: number;
}

export interface CiRun {
  id: number;
  workflow_name?: string;
  event?: string;
  branch?: string;
  commit_hash?: string;
  status?: string;
  conclusion?: string | null;
  queued_at?: string;
  started_at?: string;
  finished_at?: string;
  run_url?: string;
}

export interface CiStatusJson {
  /** e.g. "running", "success", "failure", "none". */
  state?: string;
  run?: CiRun | null;
  run_url?: string | null;
  commit?: string;
}

export interface CiRunsJson {
  runs: CiRun[];
}

export interface RepoListJson {
  repos: { full_name: string; owner: string; name: string; is_public?: boolean; updated_at?: string }[];
}

export interface OpenJson {
  url?: string;
  web_url?: string;
  review_url?: string;
}

/** `oak agent state --json --compact` (only the fields the extension reads). */
export interface AgentStateJson {
  branch?: string | null;
  head?: string | null;
  /** Server's head for this branch per the last push receipt (or `--refresh`); null if never pushed. */
  current_branch_pushed_head?: string | null;
  current_branch_push_source?: string | null;
  /** Absent means false / 0. */
  needs_push?: boolean;
  unpushed_commit_count?: number;
}
