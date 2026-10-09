import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import {
  CommitFileNode,
  CommitNode,
  HistoryProvider,
  MoreNode,
  openCommitChanges,
  openCommitFileChange,
} from './history';
import { Model } from './model';
import { Oak, OakError, OakNotFoundError } from './oak';
import { relativeTime, validateBranchName, validateRepoSpec } from './parse';
import { OakResource, Repository } from './repository';
import { BranchEntry, CiRun, RepoListJson } from './types';
import { HEAD_REF, OAK_SCHEME, fromOakUri, shortHash, toOakUri } from './uri';

type RepoArg = vscode.SourceControl | vscode.SourceControlResourceGroup | OakResource | vscode.Uri | undefined;

export class CommandCenter implements vscode.Disposable {
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly model: Model,
    private readonly oak: Oak,
    private readonly history: HistoryProvider,
    private readonly output: vscode.OutputChannel,
  ) {
    const register = (id: string, fn: (...args: any[]) => unknown) => {
      this.disposables.push(
        vscode.commands.registerCommand(id, async (...args: any[]) => {
          try {
            await fn.apply(this, args);
          } catch (err) {
            await this.showError(err);
          }
        }),
      );
    };

    register('oak.refresh', this.refresh);
    register('oak.init', this.init);
    register('oak.clone', this.clone);
    register('oak.login', this.login);
    register('oak.showOutput', () => this.output.show());

    register('oak.commit', (arg?: RepoArg) => this.commit(arg, false));
    register('oak.commitAndPush', (arg?: RepoArg) => this.commit(arg, true));
    register('oak.commitSelected', this.commitSelected);
    register('oak.saveDescription', this.saveDescription);
    register('oak.finish', this.finish);

    register('oak.openResource', this.openResource);
    register('oak.openChange', this.openChange);
    register('oak.openFile', this.openFile);
    register('oak.openHEADFile', this.openHEADFile);
    register('oak.openAllChanges', this.openAllChanges);
    register('oak.discard', this.discard);
    register('oak.discardAll', this.discardAll);

    register('oak.push', (arg?: RepoArg) => this.push(arg, false));
    register('oak.pushForce', (arg?: RepoArg) => this.push(arg, true));
    register('oak.publish', this.publish);
    register('oak.pull', (arg?: RepoArg) => this.pull(arg, false));
    register('oak.pullForce', (arg?: RepoArg) => this.pull(arg, true));
    register('oak.fetch', this.fetch);
    register('oak.sync', this.sync);
    register('oak.merge', (arg?: RepoArg) => this.merge(arg, false));
    register('oak.mergeWait', (arg?: RepoArg) => this.merge(arg, true));

    register('oak.switchBranch', this.switchBranch);
    register('oak.createBranch', (arg?: RepoArg) => this.createBranch(arg, false));
    register('oak.createBranchClean', (arg?: RepoArg) => this.createBranch(arg, true));
    register('oak.renameBranch', this.renameBranch);
    register('oak.closeBranch', this.closeBranch);

    register('oak.takeOurs', (...args: unknown[]) => this.takeSide(args, 'ours'));
    register('oak.takeTheirs', (...args: unknown[]) => this.takeSide(args, 'theirs'));
    register('oak.continue', this.continueOperation);
    register('oak.abort', this.abortOperation);

    register('oak.ci.showStatus', this.ciShowStatus);
    register('oak.ci.showRuns', this.ciShowRuns);

    register('oak.openOnWeb', this.openOnWeb);
    register('oak.openFileOnWeb', this.openFileOnWeb);
    register('oak.copyReviewUrl', this.copyReviewUrl);

    register('oak.viewFileHistory', this.viewFileHistory);
    register('oak.history.clearFilter', () => this.history.setFileFilter(undefined));
    register('oak.history.refresh', () => this.history.refresh());
    register('oak.history.loadMore', (node: MoreNode) => this.history.loadMore(node.repo));
    register('oak.history.openFileChange', (node: CommitFileNode) => openCommitFileChange(node));
    register('oak.history.openCommitChanges', (node: CommitNode) => openCommitChanges(this.history, node));
    register('oak.history.copyHash', (node: CommitNode) => vscode.env.clipboard.writeText(node.hash));
    register('oak.history.openCommitOnWeb', this.openCommitOnWeb);
    register('oak.history.switchDetached', this.switchDetached);
    register('oak.history.openFileAtRevision', this.openFileAtRevision);
    register('oak.history.compareWithWorkingTree', this.compareWithWorkingTree);
    register('oak.history.restoreFile', this.restoreFileFromCommit);
  }

  // ------------------------------------------------------------ helpers

  private async repoFor(arg: unknown): Promise<Repository | undefined> {
    if (arg instanceof OakResource) {
      return arg.repository;
    }
    if (arg && typeof arg === 'object') {
      if ('inputBox' in arg) {
        return this.model.getRepository(arg as vscode.SourceControl);
      }
      if ('resourceStates' in arg && 'id' in arg) {
        const group = arg as vscode.SourceControlResourceGroup;
        return this.model.repositories.find((r) => [r.changesGroup, r.conflictGroup, r.branchGroup].includes(group));
      }
      if (arg instanceof vscode.Uri) {
        const repo = this.model.getRepository(arg);
        if (repo) {
          return repo;
        }
      }
    }
    return this.pickRepository();
  }

  private async pickRepository(): Promise<Repository | undefined> {
    const repos = this.model.repositories;
    if (repos.length === 0) {
      const choice = await vscode.window.showWarningMessage(
        'No Oak repository is open in this workspace.',
        'Initialize Repository',
        'Clone Repository',
      );
      if (choice === 'Initialize Repository') {
        await this.init();
      } else if (choice === 'Clone Repository') {
        await this.clone();
      }
      return undefined;
    }
    if (repos.length === 1) {
      return repos[0];
    }
    const active = vscode.window.activeTextEditor?.document.uri;
    const activeRepo = active ? this.model.getRepository(active) : undefined;
    const picked = await vscode.window.showQuickPick(
      repos
        .map((r) => ({ label: r.label, description: r.branch, detail: r.root, repo: r }))
        .sort((a, b) => Number(b.repo === activeRepo) - Number(a.repo === activeRepo)),
      { placeHolder: 'Choose an Oak repository' },
    );
    return picked?.repo;
  }

  /** Resource states from a context-menu invocation (clicked + multi-selection). */
  private resourcesFrom(args: unknown[]): OakResource[] {
    const out: OakResource[] = [];
    for (const a of args) {
      if (a instanceof OakResource) {
        out.push(a);
      } else if (Array.isArray(a)) {
        out.push(...a.filter((x): x is OakResource => x instanceof OakResource));
      }
    }
    const seen = new Set<string>();
    const unique = out.filter((r) => !seen.has(r.resourceUri.toString()) && !!seen.add(r.resourceUri.toString()));
    if (unique.length > 0) {
      return unique;
    }
    // Invoked from the editor title / palette: use the active file.
    const uri = args.find((a): a is vscode.Uri => a instanceof vscode.Uri) ?? vscode.window.activeTextEditor?.document.uri;
    if (uri) {
      const target = uri.scheme === OAK_SCHEME ? vscode.Uri.file(fromOakUri(uri).path) : uri;
      const repo = this.model.getRepository(target);
      const match = repo && [...repo.conflictResources, ...repo.workingResources].find((r) => r.resourceUri.fsPath === target.fsPath);
      if (match) {
        return [match];
      }
    }
    return [];
  }

  private async showError(err: unknown): Promise<void> {
    if (err instanceof OakNotFoundError) {
      const choice = await vscode.window.showErrorMessage(err.message, 'Install Oak', 'Open Settings');
      if (choice === 'Install Oak') {
        await vscode.env.openExternal(vscode.Uri.parse('https://oak.space/docs'));
      } else if (choice === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'oak.path');
      }
      return;
    }
    if (err instanceof OakError) {
      const actions = ['Show Output'];
      const text = `${err.message}\n${err.stderr}`;
      const authProblem = err.isNetwork && /log ?in|unauthori[sz]ed|401|credential|api key/i.test(text);
      if (authProblem) {
        actions.unshift('Log In');
      }
      if (err.isConflict) {
        actions.unshift('Show Conflicts');
      }
      const choice = await vscode.window.showErrorMessage(`Oak: ${err.message}`, ...actions);
      if (choice === 'Show Output') {
        this.output.show();
      } else if (choice === 'Log In') {
        await this.login();
      } else if (choice === 'Show Conflicts') {
        await vscode.commands.executeCommand('workbench.view.scm');
      }
      return;
    }
    if (err instanceof Error && err.name === 'Canceled') {
      return;
    }
    const message = err instanceof Error ? err.message : String(err);
    const choice = await vscode.window.showErrorMessage(`Oak: ${message}`, 'Show Output');
    if (choice) {
      this.output.show();
    }
  }

  private async saveDirtyDocuments(repo: Repository): Promise<void> {
    const mode = vscode.workspace.getConfiguration('oak', vscode.Uri.file(repo.root)).get<string>('promptToSaveFilesBeforeCommit', 'always');
    if (mode === 'never') {
      return;
    }
    const dirty = vscode.workspace.textDocuments.filter((d) => d.isDirty && d.uri.scheme === 'file' && repo.contains(d.uri.fsPath));
    if (dirty.length === 0) {
      return;
    }
    if (mode === 'prompt') {
      const names = dirty.map((d) => path.basename(d.uri.fsPath)).join(', ');
      const choice = await vscode.window.showWarningMessage(
        `Save changes to ${dirty.length === 1 ? names : `${dirty.length} files`} before checkpointing?`,
        { modal: true, detail: dirty.length > 1 ? names : undefined },
        'Save All & Checkpoint',
        'Checkpoint Without Saving',
      );
      if (!choice) {
        throw new vscode.CancellationError();
      }
      if (choice === 'Checkpoint Without Saving') {
        return;
      }
    }
    await Promise.all(dirty.map((d) => d.save()));
  }

  // ------------------------------------------------------------- setup

  private async refresh(arg?: RepoArg): Promise<void> {
    if (arg) {
      const repo = await this.repoFor(arg);
      await repo?.refresh();
      return;
    }
    await this.model.scanWorkspace();
    await Promise.all(this.model.repositories.map((r) => r.refresh()));
    this.history.refresh();
  }

  private async init(): Promise<void> {
    const folders = vscode.workspace.workspaceFolders ?? [];
    let target: vscode.Uri | undefined;
    if (folders.length === 1) {
      target = folders[0].uri;
    } else {
      const items: (vscode.QuickPickItem & { uri?: vscode.Uri })[] = folders.map((f) => ({
        label: f.name,
        description: f.uri.fsPath,
        uri: f.uri,
      }));
      items.push({ label: '$(folder) Choose Folder...' });
      const picked = await vscode.window.showQuickPick(items, { placeHolder: 'Pick a folder to initialize as an Oak repository' });
      if (!picked) {
        return;
      }
      target = picked.uri;
    }
    if (!target) {
      const chosen = await vscode.window.showOpenDialog({ canSelectFolders: true, canSelectFiles: false, openLabel: 'Initialize Repository' });
      target = chosen?.[0];
    }
    if (!target) {
      return;
    }
    await this.oak.exec(['init', target.fsPath], { cwd: target.fsPath });
    const repo = await this.model.open(target.fsPath);
    void vscode.window.showInformationMessage(`Initialized an Oak repository in ${repo.root}.`);
  }

  private async clone(spec?: string): Promise<void> {
    if (!spec) {
      spec = await this.pickRemoteRepo('Repository to clone (ORG/REPO)');
    }
    if (!spec) {
      return;
    }
    const parent = await vscode.window.showOpenDialog({
      canSelectFolders: true,
      canSelectFiles: false,
      openLabel: 'Select as Clone Destination',
      defaultUri: vscode.workspace.workspaceFolders?.[0]?.uri,
    });
    if (!parent?.[0]) {
      return;
    }
    const name = spec.split('/')[1];
    const dest = path.join(parent[0].fsPath, name);
    if (fs.existsSync(dest)) {
      throw new Error(`${dest} already exists.`);
    }
    await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: `Oak: cloning ${spec}`, cancellable: true },
      async (progress, token) => {
        await this.oak.exec(['clone', spec!, dest], {
          cwd: parent[0].fsPath,
          cancel: token,
          onStderrLine: (line) => progress.report({ message: line.slice(0, 120) }),
        });
      },
    );
    const open = vscode.workspace.workspaceFolders?.length ? 'Open' : 'Open';
    const choice = await vscode.window.showInformationMessage(
      `Cloned ${spec} to ${dest}.`,
      open,
      'Open in New Window',
      'Add to Workspace',
    );
    const uri = vscode.Uri.file(dest);
    if (choice === 'Open') {
      await vscode.commands.executeCommand('vscode.openFolder', uri, { forceReuseWindow: true });
    } else if (choice === 'Open in New Window') {
      await vscode.commands.executeCommand('vscode.openFolder', uri, { forceNewWindow: true });
    } else if (choice === 'Add to Workspace') {
      vscode.workspace.updateWorkspaceFolders(vscode.workspace.workspaceFolders?.length ?? 0, 0, { uri });
    }
  }

  /** Quick pick over `oak repo list`, falling back to free text. */
  private async pickRemoteRepo(placeHolder: string, defaultValue?: string): Promise<string | undefined> {
    const qp = vscode.window.createQuickPick<vscode.QuickPickItem>();
    qp.placeholder = placeHolder;
    qp.busy = true;
    qp.value = defaultValue ?? '';
    qp.matchOnDescription = true;
    let repos: vscode.QuickPickItem[] = [];
    const cwd = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? process.cwd();
    void this.oak
      .json<RepoListJson>(['repo', 'list', '--json', '--limit', '200'], { cwd })
      .then((doc) => {
        repos = doc.repos.map((r) => ({
          label: r.full_name,
          description: r.is_public ? 'public' : 'private',
          detail: r.updated_at ? `updated ${relativeTime(r.updated_at)}` : undefined,
        }));
        qp.items = withTyped(qp.value, repos);
      })
      .catch(() => undefined)
      .finally(() => (qp.busy = false));
    qp.onDidChangeValue((v) => (qp.items = withTyped(v, repos)));
    qp.items = withTyped(qp.value, repos);
    qp.show();
    const result = await new Promise<string | undefined>((resolve) => {
      qp.onDidAccept(() => {
        const value = qp.selectedItems[0]?.label ?? qp.value;
        if (validateRepoSpec(value)) {
          qp.title = 'Expected ORG/REPO';
          return;
        }
        resolve(value.trim());
        qp.hide();
      });
      qp.onDidHide(() => resolve(undefined));
    });
    qp.dispose();
    return result;
  }

  private async login(): Promise<void> {
    const terminal = vscode.window.createTerminal({ name: 'oak login' });
    terminal.show();
    terminal.sendText(`${quoteShell(this.oak.path)} login`);
    const sub = vscode.window.onDidCloseTerminal((t) => {
      if (t === terminal) {
        sub.dispose();
        void this.refresh();
      }
    });
  }

  // ------------------------------------------------------------- commit

  private async commit(arg: RepoArg, push: boolean, resources?: OakResource[]): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    await this.saveDirtyDocuments(repo);
    await repo.refresh();
    if (repo.conflictResources.length > 0 && !resources) {
      void vscode.window.showWarningMessage('Resolve the merge conflicts before checkpointing (or abort the operation).');
      return;
    }
    if (repo.workingResources.length === 0 && (!resources || resources.length === 0)) {
      const value = repo.sourceControl.inputBox.value.trim();
      if (value && value !== (repo.status?.branch_description ?? '').trim()) {
        await repo.applyInputBoxDescription();
        void vscode.window.setStatusBarMessage('$(check) Oak: branch description saved', 3000);
        return;
      }
      if (repo.needsPush) {
        const n = repo.unpushedCount;
        void vscode.window
          .showInformationMessage(
            `There are no changes to checkpoint, but ${n > 0 ? `${n} commit${n === 1 ? '' : 's'}` : 'local commits'} on '${repo.branch}' ${n === 1 ? 'is' : 'are'} not pushed.`,
            'Push',
          )
          .then((choice) => (choice === 'Push' ? this.push(repo.sourceControl, false) : undefined))
          .then(undefined, (err) => this.showError(err));
        return;
      }
      void vscode.window.showInformationMessage('There are no changes to checkpoint.');
      return;
    }
    await repo.applyInputBoxDescription();
    const postCommit = vscode.workspace.getConfiguration('oak', vscode.Uri.file(repo.root)).get<string>('postCommitCommand', 'none');
    const wantPush = push || postCommit === 'push';
    const paths = resources?.map((r) => repo.relativePath(r.resourceUri.fsPath)) ?? [];
    const result = await repo.commit(paths, { push: wantPush && repo.isLinked });
    if (!result.committed) {
      void vscode.window.showInformationMessage('Nothing was checkpointed (no changes).');
      return;
    }
    if (wantPush) {
      void vscode.window.setStatusBarMessage(`$(check) Oak: checkpointed ${shortHash(result.head_after)}${repo.isLinked ? ' and pushed' : ''}`, 4000);
      if (!repo.isLinked) {
        await this.publish(repo.sourceControl);
      }
      return;
    }
    // Don't hold the checkpoint command open on a notification nobody may answer.
    void this.offerPushAfterCommit(repo, result.head_after ?? undefined, result.unpushed_commit_count).catch((err) => this.showError(err));
  }

  /** Non-modal "push now?" after a local-only checkpoint (setting: oak.promptToPushAfterCommit). */
  private async offerPushAfterCommit(repo: Repository, head: string | undefined, unpushed: number | undefined): Promise<void> {
    const config = vscode.workspace.getConfiguration('oak', vscode.Uri.file(repo.root));
    if (!config.get<boolean>('promptToPushAfterCommit', true)) {
      void vscode.window.setStatusBarMessage(`$(check) Oak: checkpointed ${shortHash(head)}`, 4000);
      return;
    }
    // Prefer agent state (commits since the pushed head): `commit --json`'s
    // unpushed_commit_count over-counts on an already-published branch.
    const n = repo.pushStateKnown ? repo.unpushedCount : unpushed ?? 0;
    const what = n > 0 ? `${n} unpushed commit${n === 1 ? '' : 's'}` : 'local only';
    const push = repo.isLinked ? 'Push' : 'Publish...';
    const always = 'Always Push';
    const never = "Don't Ask Again";
    const choice = await vscode.window.showInformationMessage(
      `Checkpointed ${shortHash(head)} on '${repo.branch}' (${what}).`,
      push,
      ...(repo.isLinked ? [always] : []),
      never,
    );
    if (choice === push) {
      await this.push(repo.sourceControl, false);
    } else if (choice === always) {
      await config.update('postCommitCommand', 'push', vscode.ConfigurationTarget.Global);
      await this.push(repo.sourceControl, false);
    } else if (choice === never) {
      await config.update('promptToPushAfterCommit', false, vscode.ConfigurationTarget.Global);
    }
  }

  private async commitSelected(...args: unknown[]): Promise<void> {
    const resources = this.resourcesFrom(args).filter((r) => r.kind === 'working');
    if (resources.length === 0) {
      return;
    }
    await this.commit(resources[0].repository.sourceControl, false, resources);
  }

  private async saveDescription(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    let value = repo.sourceControl.inputBox.value.trim();
    if (!value) {
      value =
        (await vscode.window.showInputBox({
          prompt: `Description for '${repo.branch}' (becomes the squash-merge message)`,
          value: repo.status?.branch_description ?? '',
        })) ?? '';
    }
    if (!value.trim()) {
      return;
    }
    await repo.setDescription(value);
    void vscode.window.setStatusBarMessage('$(check) Oak: branch description saved', 3000);
  }

  private async finish(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    await this.saveDirtyDocuments(repo);
    let desc = repo.sourceControl.inputBox.value.trim() || (repo.status?.branch_description ?? '').trim();
    if (!desc) {
      desc =
        (
          await vscode.window.showInputBox({
            prompt: `Describe what '${repo.branch}' does — this becomes the squash-merge message`,
            ignoreFocusOut: true,
          })
        )?.trim() ?? '';
    }
    if (!desc) {
      return;
    }
    const out = await repo.finish(desc);
    let review: string | undefined;
    try {
      const doc = JSON.parse(out.trim());
      review = doc.review_url ?? doc.branch_url ?? undefined;
    } catch {
      // ignore
    }
    review = review ?? repo.status?.review_url ?? undefined;
    const choice = await vscode.window.showInformationMessage(
      `Finished '${repo.branch}': description saved, work checkpointed and published.`,
      ...(review ? ['Open Review'] : []),
    );
    if (choice === 'Open Review' && review) {
      await vscode.env.openExternal(vscode.Uri.parse(review));
    }
  }

  // --------------------------------------------------------------- open

  private async openResource(resource: OakResource): Promise<void> {
    if (!(resource instanceof OakResource)) {
      return;
    }
    const repo = resource.repository;
    const name = path.basename(resource.resourceUri.fsPath);
    const status = resource.status;

    if (resource.kind === 'branch') {
      const base = repo.status?.branch_changes?.base;
      const head = repo.status?.branch_changes?.head ?? repo.head;
      if (!head) {
        return;
      }
      const right = status === 'deleted' ? undefined : toOakUri(resource.resourceUri, head);
      const left = base && status !== 'added' ? toOakUri(resource.oldUri ?? resource.resourceUri, base) : undefined;
      if (left && right) {
        await vscode.commands.executeCommand('vscode.diff', left, right, `${name} (${repo.parent} ↔ ${repo.branch})`, { preview: true });
      } else {
        await vscode.commands.executeCommand('vscode.open', right ?? left, { preview: true });
      }
      return;
    }
    if (resource.kind === 'conflict' || status === 'added') {
      await vscode.commands.executeCommand('vscode.open', resource.resourceUri, { preview: true });
      return;
    }
    if (status === 'deleted') {
      await vscode.commands.executeCommand('vscode.open', toOakUri(resource.resourceUri, HEAD_REF), { preview: true }, `${name} (Deleted)`);
      return;
    }
    const left = toOakUri(resource.oldUri ?? resource.resourceUri, HEAD_REF);
    await vscode.commands.executeCommand('vscode.diff', left, resource.resourceUri, `${name} (Working Tree)`, { preview: true });
  }

  private async openChange(...args: unknown[]): Promise<void> {
    const resources = this.resourcesFrom(args);
    if (resources.length > 0) {
      for (const r of resources) {
        await this.openResource(r);
      }
      return;
    }
    // Unchanged file: still show HEAD ↔ working tree.
    const uri = args.find((a): a is vscode.Uri => a instanceof vscode.Uri) ?? vscode.window.activeTextEditor?.document.uri;
    if (uri?.scheme === 'file' && this.model.getRepository(uri)) {
      await vscode.commands.executeCommand('vscode.diff', toOakUri(uri, HEAD_REF), uri, `${path.basename(uri.fsPath)} (Working Tree)`);
    }
  }

  private async openFile(...args: unknown[]): Promise<void> {
    const resources = args.flat().filter((a): a is OakResource => a instanceof OakResource);
    let uris = resources.filter((r) => r.status !== 'deleted').map((r) => r.resourceUri);
    if (uris.length === 0) {
      const uri = args.find((a): a is vscode.Uri => a instanceof vscode.Uri) ?? vscode.window.activeTextEditor?.document.uri;
      if (uri?.scheme === OAK_SCHEME) {
        uris = [vscode.Uri.file(fromOakUri(uri).path)];
      } else if (uri) {
        uris = [uri];
      }
    }
    for (const uri of uris) {
      await vscode.commands.executeCommand('vscode.open', uri, { preview: uris.length === 1 });
    }
  }

  private async openHEADFile(...args: unknown[]): Promise<void> {
    const resources = this.resourcesFrom(args);
    let uri: vscode.Uri | undefined = resources[0]?.oldUri ?? resources[0]?.resourceUri;
    if (!uri) {
      const active = args.find((a): a is vscode.Uri => a instanceof vscode.Uri) ?? vscode.window.activeTextEditor?.document.uri;
      uri = active?.scheme === 'file' ? active : undefined;
    }
    if (!uri) {
      return;
    }
    await vscode.commands.executeCommand('vscode.open', toOakUri(uri, HEAD_REF), { preview: true }, `${path.basename(uri.fsPath)} (HEAD)`);
  }

  private async openAllChanges(group?: vscode.SourceControlResourceGroup | vscode.SourceControl): Promise<void> {
    const repo = await this.repoFor(group);
    if (!repo) {
      return;
    }
    const isBranch = group && 'id' in group && 'resourceStates' in group && group === repo.branchGroup;
    const resources = isBranch ? (repo.branchGroup.resourceStates as OakResource[]) : repo.workingResources;
    if (resources.length === 0) {
      return;
    }
    const base = repo.status?.branch_changes?.base;
    const head = repo.status?.branch_changes?.head ?? repo.head;
    const entries = resources.map((r) => {
      if (isBranch) {
        return [
          r.resourceUri,
          base && r.status !== 'added' ? toOakUri(r.oldUri ?? r.resourceUri, base) : undefined,
          head && r.status !== 'deleted' ? toOakUri(r.resourceUri, head) : undefined,
        ];
      }
      return [
        r.resourceUri,
        r.status === 'added' ? undefined : toOakUri(r.oldUri ?? r.resourceUri, HEAD_REF),
        r.status === 'deleted' ? undefined : r.resourceUri,
      ];
    });
    const title = isBranch ? `${repo.branch} vs ${repo.parent}` : `${repo.label}: Changes`;
    await vscode.commands.executeCommand('vscode.changes', title, entries);
  }

  // ------------------------------------------------------------ discard

  private async discard(...args: unknown[]): Promise<void> {
    const resources = this.resourcesFrom(args).filter((r) => r.kind === 'working');
    if (resources.length === 0) {
      return;
    }
    const repo = resources[0].repository;
    const added = resources.filter((r) => r.status === 'added');
    const name = path.basename(resources[0].resourceUri.fsPath);
    let message: string;
    if (resources.length === 1) {
      message =
        added.length === 1
          ? `Are you sure you want to DELETE '${name}'? This new file has never been checkpointed and will be lost.`
          : `Are you sure you want to discard changes in '${name}'?`;
    } else {
      message = `Are you sure you want to discard changes in ${resources.length} files?${added.length ? ` ${added.length} new file(s) will be DELETED.` : ''}`;
    }
    const action = added.length === resources.length ? 'Delete' : 'Discard Changes';
    const choice = await vscode.window.showWarningMessage(message, { modal: true, detail: 'This is IRREVERSIBLE.' }, action);
    if (choice !== action) {
      return;
    }
    await repo.discard(resources.map((r) => repo.relativePath(r.resourceUri.fsPath)));
  }

  private async discardAll(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    const n = repo.workingResources.length;
    if (n === 0) {
      return;
    }
    const added = repo.workingResources.filter((r) => r.status === 'added').length;
    const choice = await vscode.window.showWarningMessage(
      `Are you sure you want to discard ALL ${n} change${n === 1 ? '' : 's'} in ${repo.label}?`,
      {
        modal: true,
        detail: `This resets the working tree to HEAD${added ? ` and DELETES ${added} new file(s)` : ''}. This is IRREVERSIBLE.`,
      },
      'Discard All',
    );
    if (choice === 'Discard All') {
      await repo.discardAll();
    }
  }

  // ------------------------------------------------------------- remote

  private async push(arg: RepoArg, force: boolean): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    if (!repo.isLinked) {
      await this.publish(repo.sourceControl);
      return;
    }
    if (force) {
      const choice = await vscode.window.showWarningMessage(
        `Force push '${repo.branch}'? This overwrites the remote branch history.`,
        { modal: true },
        'Force Push',
      );
      if (choice !== 'Force Push') {
        return;
      }
    }
    if (!force && repo.workingResources.length > 0) {
      // Push only sends checkpoints; offer to include the dirty files.
      const n = repo.workingResources.length;
      const choice = await vscode.window.showWarningMessage(
        `${n} uncommitted change${n === 1 ? '' : 's'} won't be pushed. Checkpoint ${n === 1 ? 'it' : 'them'} first?`,
        { modal: true },
        'Checkpoint & Push',
        'Push Commits Only',
      );
      if (!choice) {
        return;
      }
      if (choice === 'Checkpoint & Push') {
        await this.commit(repo.sourceControl, true);
        return;
      }
    }
    if (!force && repo.pushStateKnown && !repo.needsPush) {
      void vscode.window.setStatusBarMessage(`$(check) Oak: '${repo.branch}' is already pushed`, 4000);
      return;
    }
    await repo.push({ force });
    void vscode.window.setStatusBarMessage(`$(check) Oak: pushed ${repo.branch}`, 4000);
  }

  private async publish(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    if (repo.isLinked) {
      await repo.push();
      return;
    }
    let owner = '';
    try {
      owner = (await this.oak.exec(['whoami'], { cwd: repo.root })).stdout.trim().split(/\s+/).pop() ?? '';
    } catch {
      const choice = await vscode.window.showWarningMessage('You need to log in to oak.space before publishing.', 'Log In');
      if (choice === 'Log In') {
        await this.login();
      }
      return;
    }
    const spec = await this.pickRemoteRepo(
      'Publish to ORG/REPO (created on the server if it does not exist)',
      owner ? `${owner}/${path.basename(repo.root)}` : undefined,
    );
    if (!spec) {
      return;
    }
    await repo.push({ repo: spec });
    const urls = await repo.webUrls().catch(() => undefined);
    const choice = await vscode.window.showInformationMessage(`Published ${repo.label} to ${spec}.`, ...(urls?.web_url ? ['Open on oak.space'] : []));
    if (choice && urls?.web_url) {
      await vscode.env.openExternal(vscode.Uri.parse(urls.web_url));
    }
  }

  private async pull(arg: RepoArg, force: boolean): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    if (force) {
      const choice = await vscode.window.showWarningMessage(
        `Force pull '${repo.branch}'? Local commits that are not on the remote will be DISCARDED.`,
        { modal: true },
        'Force Pull',
      );
      if (choice !== 'Force Pull') {
        return;
      }
    }
    try {
      await repo.pull({ force });
    } catch (err) {
      if (err instanceof OakError && err.isConflict) {
        await this.reportConflicts(repo, 'pull');
        return;
      }
      throw err;
    }
  }

  private async fetch(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    await repo?.fetch();
  }

  private async sync(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    if (!repo.isLinked) {
      await this.publish(repo.sourceControl);
      return;
    }
    const config = vscode.workspace.getConfiguration('oak', vscode.Uri.file(repo.root));
    if (config.get<boolean>('confirmSync', true)) {
      const choice = await vscode.window.showWarningMessage(
        `This will pull '${repo.branch}' (merging in ${repo.parent}) and then push it.`,
        { modal: true },
        'OK',
        "OK, Don't Ask Again",
      );
      if (!choice) {
        return;
      }
      if (choice !== 'OK') {
        await config.update('confirmSync', false, vscode.ConfigurationTarget.Global);
      }
    }
    try {
      await repo.sync();
    } catch (err) {
      if (err instanceof OakError && err.isConflict) {
        await this.reportConflicts(repo, 'pull');
        return;
      }
      throw err;
    }
  }

  private async reportConflicts(repo: Repository, op: 'pull' | 'merge'): Promise<void> {
    await repo.refresh();
    const n = repo.conflictResources.length;
    const choice = await vscode.window.showWarningMessage(
      `The ${op} stopped with ${n || 'some'} conflict${n === 1 ? '' : 's'}. Resolve them, then continue.`,
      'Show Conflicts',
      `Abort ${op === 'pull' ? 'Pull' : 'Merge'}`,
    );
    if (choice === 'Show Conflicts') {
      await vscode.commands.executeCommand('workbench.view.scm');
      const first = repo.conflictResources[0];
      if (first) {
        await vscode.commands.executeCommand('vscode.open', first.resourceUri);
      }
    } else if (choice) {
      await repo.abortOperation();
    }
  }

  private async merge(arg: RepoArg, wait: boolean): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo?.branch) {
      return;
    }
    if (repo.branch === repo.parent || repo.branch === 'main') {
      void vscode.window.showInformationMessage(`'${repo.branch}' has no parent branch to merge into.`);
      return;
    }
    if (repo.workingResources.length > 0) {
      const choice = await vscode.window.showWarningMessage(
        `'${repo.branch}' has uncommitted changes. They will not be part of the merge.`,
        { modal: true },
        'Checkpoint, Push & Merge',
        'Merge Without Them',
      );
      if (!choice) {
        return;
      }
      if (choice === 'Checkpoint, Push & Merge') {
        await this.commit(repo.sourceControl, true);
      }
    }
    if (!wait) {
      const choice = await vscode.window.showWarningMessage(
        `Squash-merge '${repo.branch}' into ${repo.parent} on the server?`,
        {
          modal: true,
          detail: `The branch description becomes the merge message:\n\n${(repo.status?.branch_description ?? '(none)').slice(0, 400)}`,
        },
        'Merge',
        'Wait for CI, then Merge',
      );
      if (!choice) {
        return;
      }
      wait = choice !== 'Merge';
    }
    try {
      const out = await repo.merge({ wait });
      void vscode.window.showInformationMessage(firstLine(out) || `Merged '${repo.branch}' into ${repo.parent}.`);
    } catch (err) {
      if (err instanceof OakError && err.isConflict) {
        await this.reportConflicts(repo, 'merge');
        return;
      }
      if (err instanceof OakError && /\b412\b|CI/.test(err.message) && !wait) {
        const choice = await vscode.window.showErrorMessage(`Oak: ${err.message}`, 'Wait for CI, then Merge', 'Show CI Runs');
        if (choice === 'Wait for CI, then Merge') {
          await this.merge(repo.sourceControl, true);
        } else if (choice === 'Show CI Runs') {
          await this.ciShowRuns(repo.sourceControl);
        }
        return;
      }
      throw err;
    }
  }

  // ----------------------------------------------------------- branches

  private async switchBranch(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    type Item = vscode.QuickPickItem & { run?: () => Promise<void> };
    const qp = vscode.window.createQuickPick<Item>();
    qp.placeholder = `Select a branch to switch to (current: ${repo.branch ?? 'detached'})`;
    qp.matchOnDescription = true;
    qp.matchOnDetail = true;
    qp.busy = true;
    const actions: Item[] = [
      { label: '$(plus) Create new branch...', alwaysShow: true, run: () => this.createBranch(repo.sourceControl, false) },
      { label: '$(plus) Create new branch from clean main...', alwaysShow: true, run: () => this.createBranch(repo.sourceControl, true) },
    ];
    const byName = (value: string): Item => ({
      label: `$(cloud-download) Switch to '${value}'`,
      description: 'fetch from the remote if not local',
      alwaysShow: true,
      run: () => this.doSwitch(repo, value),
    });
    let branchItems: Item[] = [];
    const render = () => {
      const typed = qp.value.trim();
      const exact = branchItems.some((b) => b.label.endsWith(` ${typed}`));
      qp.items = [
        ...actions,
        ...(typed && !exact && !validateBranchName(typed) ? [byName(typed)] : []),
        ...branchItems,
      ];
    };
    qp.onDidChangeValue(render);
    render();
    qp.show();
    repo
      .branches()
      .then((branches) => {
        branchItems = branchQuickPickItems(branches, repo).map((b) =>
          b.kind === vscode.QuickPickItemKind.Separator ? b : { ...b, run: () => this.doSwitch(repo, b.name!) },
        );
        render();
      })
      .catch((err) => this.showError(err))
      .finally(() => (qp.busy = false));
    const picked = await new Promise<Item | undefined>((resolve) => {
      qp.onDidAccept(() => {
        resolve(qp.selectedItems[0]);
        qp.hide();
      });
      qp.onDidHide(() => resolve(undefined));
    });
    qp.dispose();
    await picked?.run?.();
  }

  private async doSwitch(repo: Repository, name: string): Promise<void> {
    if (name === repo.branch) {
      return;
    }
    await this.saveDirtyDocuments(repo).catch(() => undefined);
    try {
      await repo.switchBranch(name);
    } catch (err) {
      if (err instanceof OakError && err.isDirtyTree) {
        const choice = await vscode.window.showWarningMessage(
          `You have uncommitted changes on '${repo.branch}'.`,
          { modal: true, detail: err.message },
          'Checkpoint & Switch',
        );
        if (choice === 'Checkpoint & Switch') {
          await repo.commit();
          await repo.switchBranch(name);
        }
        return;
      }
      throw err;
    }
  }

  private async createBranch(arg: RepoArg, clean: boolean): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    const name = await vscode.window.showInputBox({
      prompt: clean
        ? 'New branch name (from latest main; uncommitted changes will be DISCARDED). Leave empty to generate one.'
        : 'New branch name (from latest main; uncommitted changes come along). Leave empty to generate one.',
      placeHolder: 'my-feature',
      validateInput: (v) => (v.trim() ? validateBranchName(v) : undefined),
    });
    if (name === undefined) {
      return;
    }
    if (clean && repo.workingResources.length > 0) {
      const choice = await vscode.window.showWarningMessage(
        `Discard ${repo.workingResources.length} uncommitted change(s) and start '${name || 'a new branch'}' from clean main?`,
        { modal: true },
        'Discard & Create',
      );
      if (choice !== 'Discard & Create') {
        return;
      }
    }
    await repo.createBranch(name.trim() || undefined, { clean });
  }

  private async renameBranch(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo?.branch) {
      return;
    }
    const current = repo.branch;
    const name = await vscode.window.showInputBox({
      prompt: `Rename branch '${current}'`,
      value: current,
      validateInput: (v) => (v.trim() === current ? 'Enter a different name' : validateBranchName(v)),
    });
    if (!name) {
      return;
    }
    await repo.renameBranch(current, name.trim());
  }

  private async closeBranch(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    const branches = (await repo.branches()).filter((b) => b.name !== 'main' && (b.status ?? 'open') === 'open');
    const picked = await vscode.window.showQuickPick(
      branches
        .map((b) => ({ label: b.name, description: b.current ? '(current)' : undefined, detail: firstLine(b.description ?? '') || undefined, b }))
        .sort((a, b) => Number(!!b.b.current) - Number(!!a.b.current)),
      { placeHolder: 'Select a branch to close' },
    );
    if (!picked) {
      return;
    }
    const choice = await vscode.window.showWarningMessage(
      `Close branch '${picked.label}'?`,
      { modal: true, detail: 'A closed branch is no longer offered for review or merge. It can be reopened from oak.space.' },
      'Close Branch',
    );
    if (choice !== 'Close Branch') {
      return;
    }
    await repo.closeBranch(picked.label);
  }

  // ---------------------------------------------------------- conflicts

  private async takeSide(args: unknown[], side: 'ours' | 'theirs'): Promise<void> {
    const resources = this.resourcesFrom(args).filter((r) => r.kind === 'conflict');
    for (const r of resources) {
      await r.repository.conflictTake(r.repository.relativePath(r.resourceUri.fsPath), side);
    }
  }

  private async continueOperation(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    await this.saveDirtyDocuments(repo);
    await repo.refresh();
    if (!repo.conflict?.in_progress) {
      void vscode.window.showInformationMessage('There is no merge or pull in progress.');
      return;
    }
    await repo.continueOperation();
  }

  private async abortOperation(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    await repo.refresh();
    if (!repo.conflict?.in_progress) {
      void vscode.window.showInformationMessage('There is no merge or pull in progress.');
      return;
    }
    const what = repo.conflict.kind === 'merge' ? 'merge' : 'pull';
    const choice = await vscode.window.showWarningMessage(
      `Abort the ${what} in progress? Conflict resolutions made so far will be lost.`,
      { modal: true },
      `Abort ${what === 'merge' ? 'Merge' : 'Pull'}`,
    );
    if (choice) {
      await repo.abortOperation();
    }
  }

  // ----------------------------------------------------------------- CI

  private async ciShowStatus(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    await repo.refreshCi();
    const ci = repo.ci;
    const run = ci?.run ?? undefined;
    type Item = vscode.QuickPickItem & { run: () => Thenable<unknown> };
    const items: Item[] = [];
    if (run) {
      const url = ci?.run_url ?? run.run_url;
      if (url) {
        items.push({ label: '$(link-external) Open Run on oak.space', run: () => vscode.env.openExternal(vscode.Uri.parse(url)) });
      }
      items.push({ label: '$(output) Show Logs', run: () => this.showCiLogs(repo, run) });
      if (ci?.state === 'failure') {
        items.push({ label: '$(debug-rerun) Re-run', run: () => repo.ciRerun(run.id).then((m) => vscode.window.showInformationMessage(firstLine(m) || 'CI re-run requested.')) });
      }
    }
    items.push({ label: '$(list-unordered) Show Recent Runs...', run: () => this.ciShowRuns(repo.sourceControl) });
    const picked = await vscode.window.showQuickPick(items, {
      placeHolder: `CI for ${repo.branch} @ ${shortHash(ci?.commit ?? repo.head)}: ${ci?.state ?? 'unknown'}${run ? ` (run #${run.id})` : ''}`,
    });
    await picked?.run();
  }

  private async ciShowRuns(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    const { runs } = await repo.ciRuns(30);
    const icon = (r: CiRun) =>
      r.status === 'completed'
        ? r.conclusion === 'success'
          ? '$(pass)'
          : r.conclusion === 'skipped'
            ? '$(circle-slash)'
            : '$(error)'
        : r.status === 'queued'
          ? '$(clock)'
          : '$(sync~spin)';
    const picked = await vscode.window.showQuickPick(
      runs.map((r) => ({
        label: `${icon(r)} #${r.id} ${r.workflow_name ?? ''}`,
        description: `${r.branch ?? ''} @ ${shortHash(r.commit_hash)}`,
        detail: `${r.event ?? ''} · ${r.conclusion ?? r.status ?? ''} · ${r.queued_at ? relativeTime(r.queued_at) : ''}`,
        r,
      })),
      { placeHolder: 'Recent CI runs', matchOnDescription: true },
    );
    if (!picked) {
      return;
    }
    const run = picked.r;
    const actions = ['Show Logs', ...(run.run_url ? ['Open on oak.space'] : []), ...(run.status === 'completed' ? ['Re-run'] : [])];
    const action = await vscode.window.showQuickPick(actions, { placeHolder: `Run #${run.id}` });
    if (action === 'Show Logs') {
      await this.showCiLogs(repo, run);
    } else if (action === 'Open on oak.space' && run.run_url) {
      await vscode.env.openExternal(vscode.Uri.parse(run.run_url));
    } else if (action === 'Re-run') {
      const msg = await repo.ciRerun(run.id);
      void vscode.window.showInformationMessage(firstLine(msg) || 'CI re-run requested.');
    }
  }

  private async showCiLogs(repo: Repository, run: CiRun): Promise<void> {
    const text = await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: `Oak: fetching logs for run #${run.id}` },
      () => repo.ciLogs(run.id),
    );
    const doc = await vscode.workspace.openTextDocument({ content: text, language: 'log' });
    await vscode.window.showTextDocument(doc, { preview: true });
  }

  // ---------------------------------------------------------------- web

  private async openOnWeb(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    const urls = await repo.webUrls();
    const url = urls.review_url ?? urls.url ?? urls.web_url;
    if (!url) {
      void vscode.window.showInformationMessage('This repository is not published to oak.space yet.');
      return;
    }
    await vscode.env.openExternal(vscode.Uri.parse(url));
  }

  private async openFileOnWeb(arg?: unknown): Promise<void> {
    const uri = arg instanceof vscode.Uri ? arg : arg instanceof OakResource ? arg.resourceUri : vscode.window.activeTextEditor?.document.uri;
    if (!uri) {
      return;
    }
    const target = uri.scheme === OAK_SCHEME ? vscode.Uri.file(fromOakUri(uri).path) : uri;
    const repo = this.model.getRepository(target);
    if (!repo) {
      return;
    }
    const urls = await repo.webUrls();
    if (!urls.web_url) {
      void vscode.window.showInformationMessage('This repository is not published to oak.space yet.');
      return;
    }
    const rel = repo.relativePath(target.fsPath).split('/').map(encodeURIComponent).join('/');
    let url = `${urls.web_url}/file/${rel}`;
    const line = vscode.window.activeTextEditor?.document.uri.toString() === uri.toString() ? vscode.window.activeTextEditor.selection.active.line + 1 : undefined;
    if (line) {
      url += `#L${line}`;
    }
    await vscode.env.openExternal(vscode.Uri.parse(url));
  }

  private async copyReviewUrl(arg?: RepoArg): Promise<void> {
    const repo = await this.repoFor(arg);
    if (!repo) {
      return;
    }
    const urls = await repo.webUrls();
    const url = urls.review_url ?? urls.web_url;
    if (url) {
      await vscode.env.clipboard.writeText(url);
      void vscode.window.setStatusBarMessage(`$(check) Copied ${url}`, 3000);
    }
  }

  private async openCommitOnWeb(node: CommitNode): Promise<void> {
    const urls = await node.repo.webUrls();
    if (!urls.web_url) {
      void vscode.window.showInformationMessage('This repository is not published to oak.space yet.');
      return;
    }
    await vscode.env.openExternal(vscode.Uri.parse(`${urls.web_url}/commits/${node.hash}`));
  }

  // ------------------------------------------------------------ history

  private async viewFileHistory(arg?: unknown): Promise<void> {
    let uri = arg instanceof vscode.Uri ? arg : arg instanceof OakResource ? arg.resourceUri : vscode.window.activeTextEditor?.document.uri;
    if (uri?.scheme === OAK_SCHEME) {
      uri = vscode.Uri.file(fromOakUri(uri).path);
    }
    if (!uri || !this.model.getRepository(uri)) {
      void vscode.window.showInformationMessage('Open a file in an Oak repository to view its history.');
      return;
    }
    this.history.setFileFilter(uri.fsPath);
    await vscode.commands.executeCommand('oak.history.focus');
  }

  private async switchDetached(node: CommitNode): Promise<void> {
    const choice = await vscode.window.showWarningMessage(
      `Check out ${shortHash(node.hash)} as a detached HEAD?`,
      { modal: true, detail: 'Checkpointing is refused while detached; create a branch with "Oak: Create Branch" to keep working.' },
      'Switch',
    );
    if (choice === 'Switch') {
      await node.repo.switchBranch(node.hash, { detach: true });
    }
  }

  private async openFileAtRevision(node: CommitFileNode): Promise<void> {
    if (node.file.status === 'deleted') {
      if (node.commit.parentHash) {
        await vscode.commands.executeCommand('vscode.open', toOakUri(node.uri, node.commit.parentHash));
      }
      return;
    }
    await vscode.commands.executeCommand(
      'vscode.open',
      toOakUri(node.uri, node.commit.hash),
      {},
      `${path.basename(node.file.path)} (${shortHash(node.commit.hash)})`,
    );
  }

  private async compareWithWorkingTree(node: CommitFileNode): Promise<void> {
    await vscode.commands.executeCommand(
      'vscode.diff',
      toOakUri(node.uri, node.commit.hash),
      node.uri,
      `${path.basename(node.file.path)} (${shortHash(node.commit.hash)} ↔ Working Tree)`,
    );
  }

  private async restoreFileFromCommit(node: CommitFileNode): Promise<void> {
    const rel = node.file.path;
    const deleted = node.file.status === 'deleted';
    const source = deleted ? node.commit.parentHash : node.commit.hash;
    if (!source) {
      return;
    }
    const choice = await vscode.window.showWarningMessage(
      `Overwrite '${rel}' in the working tree with its content at ${shortHash(source)}?`,
      { modal: true, detail: 'Uncommitted changes to this file will be lost.' },
      'Restore',
    );
    if (choice === 'Restore') {
      await node.commit.repo.restoreFrom([rel], source);
    }
  }

  dispose(): void {
    vscode.Disposable.from(...this.disposables).dispose();
  }
}

interface BranchItem extends vscode.QuickPickItem {
  name?: string;
}

export function branchQuickPickItems(branches: BranchEntry[], repo: Repository): BranchItem[] {
  const open = branches.filter((b) => (b.status ?? 'open') === 'open');
  const other = branches.filter((b) => (b.status ?? 'open') !== 'open');
  const toItem = (b: BranchEntry): BranchItem => ({
    label: `${b.current || b.name === repo.branch ? '$(check)' : '$(git-branch)'} ${b.name}`,
    description: [b.head ? shortHash(b.head) : '', b.status && b.status !== 'open' ? b.status : '', b.created_at ? relativeTime(b.created_at) : '']
      .filter(Boolean)
      .join(' · '),
    detail: firstLine(b.description ?? '') || undefined,
    name: b.name,
  });
  const items: BranchItem[] = [{ label: 'branches', kind: vscode.QuickPickItemKind.Separator }];
  // main first, then current, then the rest in server order.
  const sorted = [...open].sort((a, b) => rank(a, repo) - rank(b, repo));
  items.push(...sorted.map(toItem));
  if (other.length > 0) {
    items.push({ label: 'merged / closed', kind: vscode.QuickPickItemKind.Separator });
    items.push(...other.map(toItem));
  }
  return items;
}

function rank(b: BranchEntry, repo: Repository): number {
  if (b.name === 'main') {
    return 0;
  }
  if (b.current || b.name === repo.branch) {
    return 1;
  }
  return 2;
}

function withTyped(value: string, repos: vscode.QuickPickItem[]): vscode.QuickPickItem[] {
  const v = value.trim();
  if (!v || repos.some((r) => r.label === v)) {
    return repos;
  }
  return [{ label: v, description: validateRepoSpec(v) ? 'type ORG/REPO' : 'use this name', alwaysShow: true }, ...repos];
}

function firstLine(text: string): string {
  return text.trim().split(/\r?\n/)[0] ?? '';
}

function quoteShell(p: string): string {
  return /^[\w@%+=:,./-]+$/.test(p) ? p : `"${p.replace(/"/g, '\\"')}"`;
}
