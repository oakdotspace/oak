import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';
import { Oak, OakError } from './oak';
import {
  AgentStateJson,
  BranchEntry,
  CiRunsJson,
  CiStatusJson,
  CommitJson,
  ConflictStatusJson,
  EndpointDiffFile,
  EndpointDiffJson,
  LogEntry,
  OpenJson,
  StatusChange,
  StatusJson,
} from './types';
import { PushState, normalizeLog, normalizeStatusChanges, pushStateFrom, statusLetter, statusLabel } from './parse';
import { HEAD_REF, shortHash, toOakUri } from './uri';

export type ResourceKind = 'working' | 'branch' | 'conflict';

/** One changed file shown in the Source Control view. */
export class OakResource implements vscode.SourceControlResourceState {
  constructor(
    readonly repository: Repository,
    readonly kind: ResourceKind,
    readonly change: StatusChange,
    readonly resourceUri: vscode.Uri,
    readonly oldUri: vscode.Uri | undefined,
  ) {}

  get status(): string {
    return this.kind === 'conflict' ? 'conflicted' : this.change.status;
  }

  get letter(): string {
    return statusLetter(this.status);
  }

  get contextValue(): string {
    return `${this.kind}:${this.status}`;
  }

  get command(): vscode.Command {
    return {
      command: 'oak.openResource',
      title: 'Open',
      arguments: [this],
    };
  }

  get decorations(): vscode.SourceControlResourceDecorations {
    const deleted = this.status === 'deleted';
    return {
      strikeThrough: deleted,
      faded: false,
      tooltip: this.oldUri
        ? `${statusLabel(this.status)} from ${vscode.workspace.asRelativePath(this.oldUri, false)}`
        : statusLabel(this.status),
    };
  }
}

export interface OperationOptions {
  /** Title for the progress notification; omit to only show SCM progress. */
  title?: string;
  /** Show a cancellable notification instead of the SCM-view spinner. */
  notification?: boolean;
  cancellable?: boolean;
  /** Skip the refresh that normally follows an operation. */
  skipRefresh?: boolean;
}

const CI_TERMINAL = new Set(['success', 'failure', 'no_runs', 'timeout', 'cancelled']);

export class Repository implements vscode.Disposable, vscode.QuickDiffProvider {
  readonly sourceControl: vscode.SourceControl;
  readonly conflictGroup: vscode.SourceControlResourceGroup;
  readonly changesGroup: vscode.SourceControlResourceGroup;
  readonly branchGroup: vscode.SourceControlResourceGroup;

  private _status: StatusJson | undefined;
  private _conflict: ConflictStatusJson | undefined;
  private _ci: CiStatusJson | undefined;
  private _ciCommit: string | undefined;
  private _operations = 0;
  private _operationName: string | undefined;
  private _lastError: string | undefined;
  /** Push state from `oak agent state` (undefined until first read). */
  private _push: PushState | undefined;
  private refreshRemoteNext = false;

  /** Description last written into (or read for) the input box. */
  private syncedDescription = '';

  private refreshing: Promise<void> | undefined;
  private refreshQueued = false;
  private refreshTimer: NodeJS.Timeout | undefined;
  private ciTimer: NodeJS.Timeout | undefined;
  private autofetchTimer: NodeJS.Timeout | undefined;
  private disposed = false;

  private readonly disposables: vscode.Disposable[] = [];
  private readonly _onDidChangeState = new vscode.EventEmitter<void>();
  /** Fires after every status refresh. */
  readonly onDidChangeState = this._onDidChangeState.event;
  private readonly _onDidChangeHead = new vscode.EventEmitter<string | undefined>();
  /** Fires when HEAD moves (commit, switch, pull, reset...). */
  readonly onDidChangeHead = this._onDidChangeHead.event;
  private readonly _onDidRunOperation = new vscode.EventEmitter<string>();
  readonly onDidRunOperation = this._onDidRunOperation.event;

  constructor(
    readonly root: string,
    readonly oak: Oak,
    private readonly output: vscode.OutputChannel,
  ) {
    const rootUri = vscode.Uri.file(root);
    this.sourceControl = vscode.scm.createSourceControl('oak', 'Oak', rootUri);
    this.sourceControl.quickDiffProvider = this;
    this.sourceControl.acceptInputCommand = {
      command: 'oak.commit',
      title: 'Checkpoint',
      arguments: [this.sourceControl],
    };
    this.sourceControl.inputBox.placeholder = 'Branch description (becomes the squash-merge message)';
    this.disposables.push(this.sourceControl);

    this.conflictGroup = this.sourceControl.createResourceGroup('conflicts', 'Merge Conflicts');
    this.changesGroup = this.sourceControl.createResourceGroup('changes', 'Changes');
    this.branchGroup = this.sourceControl.createResourceGroup('branch', 'Branch Changes');
    for (const g of [this.conflictGroup, this.branchGroup]) {
      g.hideWhenEmpty = true;
    }
    this.disposables.push(this.conflictGroup, this.changesGroup, this.branchGroup);
    this.disposables.push(this._onDidChangeState, this._onDidChangeHead, this._onDidRunOperation);

    this.setupWatchers();
    this.disposables.push(
      vscode.window.onDidChangeWindowState((e) => {
        if (e.focused) {
          this.scheduleRefresh(100);
        }
      }),
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (e.affectsConfiguration('oak', rootUri)) {
          this.configureAutofetch();
          this.render();
          this.scheduleRefresh(0);
        }
      }),
    );
    this.configureAutofetch();
    this.render();
  }

  // ---------------------------------------------------------------- state

  get status(): StatusJson | undefined {
    return this._status;
  }
  get conflict(): ConflictStatusJson | undefined {
    return this._conflict;
  }
  get ci(): CiStatusJson | undefined {
    return this._ci;
  }
  get branch(): string | undefined {
    return this._status?.branch ?? undefined;
  }
  get head(): string | undefined {
    return this._status?.head ?? undefined;
  }
  get parent(): string {
    return this._status?.parent ?? 'main';
  }
  get isLinked(): boolean {
    return !!(this._status?.repo_owner && this._status?.repo_name);
  }
  get repoFullName(): string | undefined {
    return this.isLinked ? `${this._status!.repo_owner}/${this._status!.repo_name}` : undefined;
  }
  get operationInProgress(): boolean {
    return this._operations > 0;
  }
  /** True when the branch has local commits the server doesn't have. */
  get needsPush(): boolean {
    return this.isLinked && !!this._push?.needsPush;
  }
  /** Whether `oak agent state` has reported push state for the current branch. */
  get pushStateKnown(): boolean {
    return this._push !== undefined;
  }
  get unpushedCount(): number {
    return this.needsPush ? this._push!.unpushed : 0;
  }
  get lastError(): string | undefined {
    return this._lastError;
  }
  get workingResources(): OakResource[] {
    return this.changesGroup.resourceStates as OakResource[];
  }
  get conflictResources(): OakResource[] {
    return this.conflictGroup.resourceStates as OakResource[];
  }
  get label(): string {
    return path.basename(this.root);
  }

  private config(): vscode.WorkspaceConfiguration {
    return vscode.workspace.getConfiguration('oak', vscode.Uri.file(this.root));
  }

  /** Path relative to the repository root, with forward slashes. */
  relativePath(fsPath: string): string {
    return path.relative(this.root, fsPath).split(path.sep).join('/');
  }

  absolute(relPath: string): vscode.Uri {
    return vscode.Uri.file(path.join(this.root, ...relPath.split('/')));
  }

  contains(fsPath: string): boolean {
    const rel = path.relative(this.root, fsPath);
    return !rel.startsWith('..') && !path.isAbsolute(rel);
  }

  // --------------------------------------------------------- quick diff

  provideOriginalResource(uri: vscode.Uri): vscode.Uri | undefined {
    if (uri.scheme !== 'file' || !this.contains(uri.fsPath)) {
      return undefined;
    }
    const rel = this.relativePath(uri.fsPath);
    if (rel.startsWith('.oak/') || !this.head) {
      return undefined;
    }
    const change = this.findChange(rel);
    if (change?.status === 'added') {
      return undefined;
    }
    const original = change?.old_path ? this.absolute(change.old_path) : uri;
    return toOakUri(original, HEAD_REF);
  }

  findChange(rel: string): StatusChange | undefined {
    return this._status?.changes.find((c) => c.path === rel);
  }

  // ------------------------------------------------------------ refresh

  private setupWatchers(): void {
    if (!this.config().get<boolean>('autoRefresh', true)) {
      return;
    }
    const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(vscode.Uri.file(this.root), '**'));
    const onEvent = (uri: vscode.Uri) => {
      const rel = this.relativePath(uri.fsPath);
      if (rel === '.oak' || rel.startsWith('.oak/')) {
        // Only the metadata database moves on commit/switch/pull; ignore
        // lock files, publication receipts and SQLite scratch files.
        if (rel !== '.oak/oak.db' && rel !== '.oak/oak.db-wal') {
          return;
        }
      }
      if (this.operationInProgress) {
        return; // refreshed when the operation finishes
      }
      this.scheduleRefresh(400);
    };
    watcher.onDidChange(onEvent, undefined, this.disposables);
    watcher.onDidCreate(onEvent, undefined, this.disposables);
    watcher.onDidDelete(onEvent, undefined, this.disposables);
    this.disposables.push(watcher);
  }

  scheduleRefresh(delay: number): void {
    if (this.disposed) {
      return;
    }
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
    }
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = undefined;
      void this.refresh();
    }, delay);
  }

  /** Re-read status; concurrent calls coalesce into at most one follow-up. */
  async refresh(): Promise<void> {
    if (this.refreshing) {
      this.refreshQueued = true;
      return this.refreshing;
    }
    this.refreshing = (async () => {
      try {
        do {
          this.refreshQueued = false;
          await this.doRefresh();
        } while (this.refreshQueued && !this.disposed);
      } finally {
        this.refreshing = undefined;
      }
    })();
    return this.refreshing;
  }

  private async doRefresh(): Promise<void> {
    const cwd = this.root;
    const previousHead = this.head;
    try {
      // After network operations ask the server for the branch head; otherwise
      // agent state answers from the local push receipt without a request.
      const stateArgs = ['agent', 'state', '--json'];
      if (this.refreshRemoteNext) {
        this.refreshRemoteNext = false;
        stateArgs.push('--refresh');
      }
      const [status, conflict, agentState] = await Promise.all([
        this.oak.json<StatusJson>(['status', '--json'], { cwd }),
        this.oak
          .json<ConflictStatusJson>(['conflict', 'status', '--json'], { cwd, allowFailure: true })
          .catch(() => undefined),
        this.oak.json<AgentStateJson>(stateArgs, { cwd, allowFailure: true }).catch(() => undefined),
      ]);
      if (this.disposed) {
        return;
      }
      this._status = status;
      this._conflict = conflict;
      this._lastError = undefined;
      this._push = pushStateFrom(agentState, status.branch);
    } catch (err) {
      this._lastError = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`status failed: ${this._lastError}`);
    }
    this.render();
    if (previousHead !== this.head) {
      this._onDidChangeHead.fire(this.head);
    }
    this._onDidChangeState.fire();
    if (this.head && this.head !== this._ciCommit) {
      void this.refreshCi();
    }
  }

  /** Next refresh asks the server for the branch head (after pull/fetch/push). */
  private markRemoteChanged(): void {
    this.refreshRemoteNext = true;
  }

  private render(): void {
    const status = this._status;
    const conflictPaths = new Set(this._conflict?.in_progress ? this._conflict.conflict_paths : []);

    const working = normalizeStatusChanges(status?.working_changes?.changes ?? status?.changes ?? []);
    const conflictResources: OakResource[] = [];
    const workingResources: OakResource[] = [];
    for (const change of working) {
      const resource = this.toResource(conflictPaths.has(change.path) ? 'conflict' : 'working', change);
      (resource.kind === 'conflict' ? conflictResources : workingResources).push(resource);
      conflictPaths.delete(change.path);
    }
    for (const p of conflictPaths) {
      conflictResources.push(this.toResource('conflict', { path: p, status: 'conflicted' }));
    }
    this.conflictGroup.resourceStates = conflictResources;
    this.changesGroup.resourceStates = workingResources;

    const showBranch = this.config().get<boolean>('showBranchChanges', true);
    const branchChanges = showBranch ? normalizeStatusChanges(status?.branch_changes?.changes ?? []) : [];
    this.branchGroup.resourceStates = branchChanges.map((c) => this.toResource('branch', c));
    this.branchGroup.label = status?.parent ? `Branch Changes (vs ${status.parent})` : 'Branch Changes';

    const badge = this.config().get<string>('countBadge', 'all');
    this.sourceControl.count = badge === 'off' ? 0 : workingResources.length + conflictResources.length;
    this.sourceControl.statusBarCommands = this.statusBarCommands();
    this.syncInputBox();
  }

  private toResource(kind: ResourceKind, change: StatusChange): OakResource {
    const oldPath = change.old_path ?? change.from;
    return new OakResource(this, kind, change, this.absolute(change.path), oldPath ? this.absolute(oldPath) : undefined);
  }

  /** Keep the input box showing the branch description unless the user is editing it. */
  private syncInputBox(): void {
    const description = (this._status?.branch_description ?? '').trim();
    const box = this.sourceControl.inputBox;
    if (box.value.trim() === this.syncedDescription.trim()) {
      box.value = description;
    }
    this.syncedDescription = description;
    const branch = this.branch ?? 'detached HEAD';
    const key = process.platform === 'darwin' ? '⌘Enter' : 'Ctrl+Enter';
    box.placeholder = `Description of '${branch}' (${key} to checkpoint)`;
  }

  private statusBarCommands(): vscode.Command[] {
    const commands: vscode.Command[] = [];
    const status = this._status;
    if (!status) {
      if (this._lastError) {
        commands.push({ command: 'oak.showOutput', title: '$(warning) Oak', tooltip: this._lastError });
      }
      return commands;
    }
    const dirty = this.workingResources.length > 0 ? '*' : '';
    const merging = this._conflict?.in_progress
      ? this._conflict.kind === 'merge'
        ? ' (merging)'
        : ' (pulling)'
      : '';
    const branchLabel = status.branch ?? `${shortHash(status.head)} (detached)`;
    commands.push({
      command: 'oak.switchBranch',
      title: `$(git-branch) ${branchLabel}${dirty}${merging}`,
      tooltip: [
        `Oak: ${this.repoFullName ?? 'not linked to a remote'}`,
        `Branch: ${branchLabel} → ${this.parent}`,
        status.head ? `HEAD: ${shortHash(status.head)}` : 'No commits yet',
        status.unmerged_commit_count ? `${status.unmerged_commit_count} commit(s) not yet merged into ${this.parent}` : '',
        '',
        'Switch Branch...',
      ]
        .filter((l, i, a) => l !== '' || (i > 0 && a[i - 1] !== ''))
        .join('\n'),
      arguments: [this.sourceControl],
    });
    if (this.operationInProgress) {
      commands.push({
        command: 'oak.showOutput',
        title: '$(sync~spin)',
        tooltip: `Oak: ${this._operationName ?? 'running'}...`,
      });
    } else if (this.isLinked) {
      commands.push({
        command: 'oak.sync',
        title: '$(sync)',
        tooltip: `Synchronize: pull ${status.branch ?? ''} (and merge in ${this.parent}), then push`,
        arguments: [this.sourceControl],
      });
      if (this.needsPush) {
        const n = this.unpushedCount;
        commands.push({
          command: 'oak.push',
          title: n > 0 ? `$(repo-push) ${n}` : '$(repo-push)',
          tooltip: `Push ${n > 0 ? `${n} commit${n === 1 ? '' : 's'}` : 'local commits'} on '${status.branch}' to oak.space${
            this._push?.published ? '' : ' (first push of this branch)'
          }`,
          arguments: [this.sourceControl],
        });
      }
    } else {
      commands.push({
        command: 'oak.publish',
        title: '$(cloud-upload)',
        tooltip: 'Publish to oak.space (push --repo ORG/REPO)',
        arguments: [this.sourceControl],
      });
    }
    const ci = this.ciStatusBarCommand();
    if (ci) {
      commands.push(ci);
    }
    return commands;
  }

  // ------------------------------------------------------------------ CI

  private ciStatusBarCommand(): vscode.Command | undefined {
    if (!this.config().get<boolean>('ci.enabled', true) || !this._ci) {
      return undefined;
    }
    const state = this._ci.state ?? 'unknown';
    const icon: Record<string, string> = {
      success: '$(pass)',
      failure: '$(error)',
      running: '$(sync~spin)',
      no_runs: '$(circle-slash)',
      timeout: '$(watch)',
    };
    if (state === 'no_runs') {
      return undefined;
    }
    const run = this._ci.run;
    return {
      command: 'oak.ci.showStatus',
      title: `${icon[state] ?? '$(question)'} CI`,
      tooltip: `CI for ${shortHash(this._ci.commit ?? this.head)}: ${state}${run ? ` (run #${run.id}, ${run.workflow_name ?? 'ci'})` : ''}`,
      arguments: [this.sourceControl],
    };
  }

  async refreshCi(): Promise<void> {
    if (this.ciTimer) {
      clearTimeout(this.ciTimer);
      this.ciTimer = undefined;
    }
    if (!this.config().get<boolean>('ci.enabled', true) || !this.isLinked || !this.head) {
      if (this._ci) {
        this._ci = undefined;
        this.render();
      }
      return;
    }
    const head = this.head;
    try {
      // Exit codes: 0 passed, 1 failed/no runs, 3 running — all carry JSON.
      const result = await this.oak.exec(['ci', 'status', '--json'], { cwd: this.root, allowFailure: true });
      const doc = JSON.parse(result.stdout.trim()) as CiStatusJson;
      this._ci = doc;
      this._ciCommit = head;
    } catch {
      this._ci = undefined;
      this._ciCommit = head;
    }
    if (this.disposed) {
      return;
    }
    this.render();
    this._onDidChangeState.fire();
    const state = this._ci?.state;
    if (state && !CI_TERMINAL.has(state)) {
      const interval = Math.max(5, this.config().get<number>('ci.pollInterval', 20));
      this.ciTimer = setTimeout(() => void this.refreshCi(), interval * 1000);
    }
  }

  // ----------------------------------------------------------- autofetch

  private configureAutofetch(): void {
    if (this.autofetchTimer) {
      clearInterval(this.autofetchTimer);
      this.autofetchTimer = undefined;
    }
    if (!this.config().get<boolean>('autofetch', false)) {
      return;
    }
    const period = Math.max(30, this.config().get<number>('autofetchPeriod', 180));
    this.autofetchTimer = setInterval(() => {
      if (!this.operationInProgress && this.isLinked && vscode.window.state.focused) {
        void this.run('Fetching', () => this.oak.exec(['fetch'], { cwd: this.root }), {}).catch(() => undefined);
      }
    }, period * 1000);
  }

  // ---------------------------------------------------------- operations

  /**
   * Run a mutating operation with progress in the SCM view, then refresh.
   * Errors propagate to the caller (commands show them).
   */
  async run<T>(name: string, fn: (token?: vscode.CancellationToken) => Promise<T>, options: OperationOptions = {}): Promise<T> {
    this._operations++;
    this._operationName = name;
    this.render();
    try {
      if (options.notification) {
        return await vscode.window.withProgress(
          { location: vscode.ProgressLocation.Notification, title: options.title ?? name, cancellable: options.cancellable },
          (_progress, token) => fn(token),
        );
      }
      return await vscode.window.withProgress({ location: vscode.ProgressLocation.SourceControl }, () => fn());
    } finally {
      this._operations--;
      if (this._operations === 0) {
        this._operationName = undefined;
      }
      this._onDidRunOperation.fire(name);
      if (!options.skipRefresh) {
        await this.refresh();
      } else {
        this.render();
      }
    }
  }

  private exec(args: string[], extra: Partial<Parameters<Oak['exec']>[1]> = {}) {
    return this.oak.exec(args, { cwd: this.root, ...extra });
  }

  /** Checkpoint the whole working tree, or only `paths`. */
  async commit(paths: string[] = [], opts: { push?: boolean } = {}): Promise<CommitJson> {
    const args = ['commit', '--json'];
    if (opts.push) {
      args.push('--push');
    }
    if (paths.length > 0) {
      args.push('--', ...paths);
    }
    if (opts.push) {
      this.markRemoteChanged();
    }
    const label = opts.push ? 'Checkpointing and pushing' : 'Checkpointing';
    return this.run(label, async () => {
      const result = await this.exec(args);
      return JSON.parse(result.stdout.trim().split(/\r?\n/).pop() ?? '{}') as CommitJson;
    }, opts.push ? { notification: true, title: 'Oak: checkpoint and push' } : {});
  }

  /** Save the branch description (`oak desc --file -`). */
  async setDescription(text: string): Promise<void> {
    await this.run('Saving description', () => this.exec(['desc', '--file', '-'], { input: text }));
    this.syncedDescription = text.trim();
  }

  /** Apply the input box as the branch description if the user edited it. */
  async applyInputBoxDescription(): Promise<void> {
    const value = this.sourceControl.inputBox.value.trim();
    const current = (this._status?.branch_description ?? '').trim();
    if (value && value !== current) {
      await this.setDescription(value);
    }
  }

  async discard(relPaths: string[]): Promise<void> {
    if (relPaths.length === 0) {
      return;
    }
    await this.run('Discarding', () => this.exec(['restore', '-f', '--', ...relPaths]));
  }

  async discardAll(): Promise<void> {
    await this.run('Discarding all changes', () => this.exec(['reset', '-f']));
  }

  async restoreFrom(relPaths: string[], source: string): Promise<void> {
    await this.run('Restoring', () => this.exec(['restore', '-f', '-s', source, '--', ...relPaths]));
  }

  async push(opts: { force?: boolean; repo?: string } = {}): Promise<void> {
    const args = ['push'];
    if (opts.force) {
      args.push('--force');
    }
    if (opts.repo) {
      args.push('--repo', opts.repo);
    }
    this.markRemoteChanged();
    await this.run('Pushing', (token) => this.exec(args, { cancel: token }), {
      notification: true,
      title: opts.repo ? `Oak: publishing to ${opts.repo}` : `Oak: pushing ${this.branch ?? ''}`,
    });
    void this.refreshCi();
  }

  async pull(opts: { force?: boolean } = {}): Promise<void> {
    const args = ['pull'];
    if (opts.force) {
      args.push('--force');
    }
    this.markRemoteChanged();
    await this.run('Pulling', (token) => this.exec(args, { cancel: token }), {
      notification: true,
      title: `Oak: pulling ${this.branch ?? ''}`,
    });
  }

  async fetch(): Promise<void> {
    this.markRemoteChanged();
    await this.run('Fetching', (token) => this.exec(['fetch'], { cancel: token }), {
      notification: true,
      title: `Oak: fetching ${this.parent}`,
    });
  }

  async sync(): Promise<void> {
    this.markRemoteChanged();
    await this.run(
      'Synchronizing',
      async (token) => {
        await this.exec(['pull'], { cancel: token });
        await this.exec(['push'], { cancel: token });
      },
      { notification: true, title: `Oak: synchronizing ${this.branch ?? ''}` },
    );
    void this.refreshCi();
  }

  async merge(opts: { wait?: boolean; force?: boolean } = {}): Promise<string> {
    this.markRemoteChanged();
    const args = ['merge'];
    if (opts.force) {
      args.push('--force');
    }
    if (opts.wait) {
      args.push('--wait');
    }
    return this.run(
      'Merging',
      async (token) => {
        const r = await this.exec(args, { cancel: token });
        return r.stdout.trim() || r.stderr.trim();
      },
      {
        notification: true,
        cancellable: true,
        title: opts.wait ? `Oak: waiting for CI, then merging ${this.branch} into ${this.parent}` : `Oak: merging ${this.branch} into ${this.parent}`,
      },
    );
  }

  async finish(description: string): Promise<string> {
    this.markRemoteChanged();
    return this.run(
      'Finishing',
      async (token) => {
        const r = await this.exec(['finish', '--desc-file', '-', '--json'], { input: description, cancel: token });
        return r.stdout;
      },
      { notification: true, title: `Oak: finishing ${this.branch}` },
    );
  }

  async switchBranch(name: string, opts: { detach?: boolean } = {}): Promise<void> {
    this.markRemoteChanged();
    const args = ['switch', name];
    if (opts.detach) {
      args.push('--detach');
    }
    await this.run(`Switching to ${name}`, (token) => this.exec(args, { cancel: token }), {
      notification: true,
      title: `Oak: switching to ${opts.detach ? shortHash(name) : name}`,
    });
  }

  async createBranch(name: string | undefined, opts: { clean?: boolean } = {}): Promise<void> {
    const args = ['switch', '-c'];
    if (name) {
      args.push(name);
    }
    if (opts.clean) {
      args.push('--clean');
    }
    await this.run('Creating branch', (token) => this.exec(args, { cancel: token }), {
      notification: true,
      title: `Oak: creating branch ${name ?? ''}`.trim(),
    });
  }

  async renameBranch(oldName: string, newName: string): Promise<void> {
    await this.run('Renaming branch', () => this.exec(['branch', 'rename', oldName, newName]));
  }

  async closeBranch(name: string, opts: { remote?: boolean; reason?: string } = {}): Promise<void> {
    const args = ['close', name];
    if (opts.remote) {
      args.push('--remote');
    }
    if (opts.reason) {
      args.push('--reason', opts.reason);
    }
    await this.run('Closing branch', () => this.exec(args), { notification: true, title: `Oak: closing ${name}` });
  }

  async conflictTake(relPath: string, side: 'ours' | 'theirs'): Promise<void> {
    await this.run('Resolving conflict', () => this.exec(['conflict', 'take', `--${side}`, '--', relPath]));
  }

  async continueOperation(): Promise<void> {
    const kind = this._conflict?.kind;
    const cmd = kind === 'merge' ? 'merge' : 'pull';
    await this.run('Continuing', (token) => this.exec([cmd, '--continue'], { cancel: token }), {
      notification: true,
      title: `Oak: continuing ${cmd}`,
    });
  }

  async abortOperation(): Promise<void> {
    const kind = this._conflict?.kind;
    const cmd = kind === 'merge' ? 'merge' : 'pull';
    await this.run('Aborting', () => this.exec([cmd, '--abort']));
  }

  // -------------------------------------------------------------- queries

  async log(opts: { limit?: number; paths?: string[] } = {}): Promise<LogEntry[]> {
    const args = ['log', '--json'];
    if (opts.limit) {
      args.push('-n', String(opts.limit));
    }
    if (opts.paths?.length) {
      args.push('--', ...opts.paths);
    }
    try {
      const raw = await this.oak.json<unknown>(args, { cwd: this.root });
      return normalizeLog(raw);
    } catch (err) {
      // A repository with no commits yet has no history.
      if (err instanceof OakError && !this.head) {
        return [];
      }
      throw err;
    }
  }

  async branches(): Promise<BranchEntry[]> {
    const raw = await this.oak.json<BranchEntry[] | { branches: BranchEntry[] }>(['branch', 'list', '--json'], {
      cwd: this.root,
    });
    return Array.isArray(raw) ? raw : raw.branches ?? [];
  }

  /** Files changed between two revisions (`oak diff A B --json`), all pages. */
  async diffFiles(from: string, to: string): Promise<EndpointDiffFile[]> {
    const files: EndpointDiffFile[] = [];
    let offset = 0;
    for (let page = 0; page < 50; page++) {
      const args = ['diff', from, to, '--json', '--changed-files-limit', '500', '--changed-files-offset', String(offset)];
      const doc = await this.oak.json<EndpointDiffJson>(args, { cwd: this.root });
      files.push(...(doc.changed_files ?? []));
      const next = doc.changed_files_page?.next_offset;
      if (next === undefined || next === null || next <= offset) {
        break;
      }
      offset = next;
    }
    return files;
  }

  /** All files in a commit's tree (used for root commits, which have no parent to diff). */
  async treeFiles(commit: string): Promise<string[]> {
    const doc = await this.oak.json<{ evidence?: { files?: { path: string }[] } }>(
      ['tree', 'inspect', '--at', commit, '--json', '--max-files', '5000'],
      { cwd: this.root, allowFailure: true },
    );
    return (doc.evidence?.files ?? []).map((e) => e.path);
  }

  /**
   * Bytes of `relPath` at `commit` (a full hash), via `oak file inspect --output`
   * (which verifies the content against the commit). Returns undefined when the
   * path does not exist at that commit.
   */
  async fileContentAt(relPath: string, commit: string): Promise<Buffer | undefined> {
    const dir = await fs.promises.mkdtemp(path.join(os.tmpdir(), 'oak-vscode-'));
    const out = path.join(dir, 'content');
    try {
      const result = await this.oak.exec(['file', 'inspect', '--at', commit, '--json', '--output', out, '--', relPath], {
        cwd: this.root,
        allowFailure: true,
      });
      if (result.exitCode !== 0) {
        let status: string | undefined;
        try {
          status = JSON.parse(result.stdout.trim())?.evidence?.status;
        } catch {
          // ignore
        }
        if (status === 'path_missing') {
          return undefined;
        }
        throw new OakError(['file', 'inspect', relPath], result.exitCode, result.stdout, result.stderr);
      }
      return await fs.promises.readFile(out);
    } finally {
      await fs.promises.rm(dir, { recursive: true, force: true });
    }
  }

  async ciRuns(limit = 20): Promise<CiRunsJson> {
    return this.oak.json<CiRunsJson>(['ci', 'runs', '--limit', String(limit), '--json'], { cwd: this.root });
  }

  async ciLogs(runId: number): Promise<string> {
    const r = await this.exec(['ci', 'logs', String(runId)]);
    return r.stdout;
  }

  async ciRerun(runId: number): Promise<string> {
    const r = await this.run('Re-running CI', () => this.exec(['ci', 'rerun', String(runId)]), { skipRefresh: true });
    void this.refreshCi();
    return r.stdout.trim();
  }

  async webUrls(): Promise<OpenJson> {
    return this.oak.json<OpenJson>(['open', '--json'], { cwd: this.root });
  }

  dispose(): void {
    this.disposed = true;
    for (const t of [this.refreshTimer, this.ciTimer]) {
      if (t) {
        clearTimeout(t);
      }
    }
    if (this.autofetchTimer) {
      clearInterval(this.autofetchTimer);
    }
    vscode.Disposable.from(...this.disposables).dispose();
  }
}
