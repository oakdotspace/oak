import * as path from 'path';
import * as vscode from 'vscode';
import { Model } from './model';
import { commitSubject, relativeTime, statusLabel, statusLetter } from './parse';
import { Repository } from './repository';
import { EndpointDiffFile, LogEntry } from './types';
import { shortHash, toOakUri } from './uri';

export class RepoNode {
  constructor(readonly repo: Repository) {}
}

export class CommitNode {
  constructor(
    readonly repo: Repository,
    readonly entry: LogEntry,
    /** Next-older commit in the listing; undefined for a root commit. */
    readonly parentHash: string | undefined,
    readonly isHead: boolean,
  ) {}
  get hash(): string {
    return this.entry.hash;
  }
}

export class CommitFileNode {
  constructor(
    readonly commit: CommitNode,
    readonly file: EndpointDiffFile,
  ) {}
  get uri(): vscode.Uri {
    return this.commit.repo.absolute(this.file.path);
  }
}

export class MoreNode {
  constructor(readonly repo: Repository) {}
}

export class MessageNode {
  constructor(readonly message: string) {}
}

export type HistoryNode = RepoNode | CommitNode | CommitFileNode | MoreNode | MessageNode;

/** The "Oak History" view in the Source Control container. */
export class HistoryProvider implements vscode.TreeDataProvider<HistoryNode>, vscode.Disposable {
  private readonly _onDidChange = new vscode.EventEmitter<HistoryNode | undefined>();
  readonly onDidChangeTreeData = this._onDidChange.event;
  private readonly limits = new Map<Repository, number>();
  /** Absolute path the view is filtered to, if any. */
  private fileFilter: string | undefined;
  private readonly disposables: vscode.Disposable[] = [];
  readonly view: vscode.TreeView<HistoryNode>;
  private refreshTimer: NodeJS.Timeout | undefined;

  constructor(private readonly model: Model) {
    this.view = vscode.window.createTreeView('oak.history', { treeDataProvider: this, showCollapseAll: true });
    this.disposables.push(
      this.view,
      this._onDidChange,
      model.onDidOpenRepository((repo) => {
        repo.onDidChangeHead(() => this.refresh(), undefined, this.disposables);
        this.refresh();
      }),
      model.onDidCloseRepository(() => this.refresh()),
    );
    for (const repo of model.repositories) {
      repo.onDidChangeHead(() => this.refresh(), undefined, this.disposables);
    }
    this.updateTitle();
  }

  refresh(): void {
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
    }
    this.refreshTimer = setTimeout(() => this._onDidChange.fire(undefined), 150);
  }

  setFileFilter(fsPath: string | undefined): void {
    this.fileFilter = fsPath;
    this.limits.clear();
    void vscode.commands.executeCommand('setContext', 'oak.historyFiltered', !!fsPath);
    this.updateTitle();
    this._onDidChange.fire(undefined);
  }

  private updateTitle(): void {
    this.view.description = this.fileFilter ? path.basename(this.fileFilter) : undefined;
    this.view.message = this.fileFilter ? `History of ${vscode.workspace.asRelativePath(this.fileFilter)}` : undefined;
  }

  loadMore(repo: Repository): void {
    this.limits.set(repo, this.limit(repo) * 2);
    this._onDidChange.fire(undefined);
  }

  private limit(repo: Repository): number {
    return this.limits.get(repo) ?? vscode.workspace.getConfiguration('oak').get<number>('historyPageSize', 50);
  }

  getTreeItem(node: HistoryNode): vscode.TreeItem {
    if (node instanceof RepoNode) {
      const item = new vscode.TreeItem(node.repo.label, vscode.TreeItemCollapsibleState.Expanded);
      item.description = node.repo.branch;
      item.iconPath = new vscode.ThemeIcon('repo');
      item.contextValue = 'repository';
      return item;
    }
    if (node instanceof CommitNode) {
      const e = node.entry;
      const item = new vscode.TreeItem(commitSubject(e), vscode.TreeItemCollapsibleState.Collapsed);
      const when = relativeTime(e.timestamp);
      item.description = `${shortHash(e.hash)} · ${e.branch ?? ''}${e.branch ? ' · ' : ''}${when}`;
      item.iconPath = new vscode.ThemeIcon(node.isHead ? 'git-commit' : 'circle-outline');
      const md = new vscode.MarkdownString(undefined, true);
      md.appendMarkdown(`**${shortHash(e.hash)}**${node.isHead ? ' (HEAD)' : ''}  \n`);
      if (e.branch) {
        md.appendMarkdown(`$(git-branch) ${e.branch}  \n`);
      }
      if (e.author) {
        md.appendMarkdown(`$(account) ${e.author}  \n`);
      }
      md.appendMarkdown(`$(history) ${new Date(e.timestamp).toLocaleString()} (${when})  \n`);
      if (e.files_changed !== undefined) {
        md.appendMarkdown(`$(files) ${e.files_changed} file${e.files_changed === 1 ? '' : 's'} changed\n\n`);
      }
      if (e.description_or_subject) {
        md.appendMarkdown('---\n\n');
        md.appendText(e.description_or_subject);
      }
      item.tooltip = md;
      item.contextValue = node.isHead ? 'commit:head' : 'commit';
      item.id = `${node.repo.root}#${e.hash}`;
      return item;
    }
    if (node instanceof CommitFileNode) {
      const f = node.file;
      // An oak: URI picks the file-type icon without the working-tree badge.
      const item = new vscode.TreeItem(toOakUri(node.uri, node.commit.hash), vscode.TreeItemCollapsibleState.None);
      item.label = path.basename(f.path);
      const dir = path.dirname(f.path);
      const stats = [f.additions ? `+${f.additions}` : '', f.deletions ? `-${f.deletions}` : ''].filter(Boolean).join(' ');
      // File decorations reflect the working tree, not this commit, so the
      // commit's status letter goes in the description instead.
      item.description = [statusLetter(f.status), dir === '.' ? '' : dir, stats].filter(Boolean).join('  ');
      item.tooltip = `${f.path} — ${statusLabel(f.status)}`;
      item.iconPath = vscode.ThemeIcon.File;
      item.contextValue = `commitFile:${f.status}`;
      item.command = { command: 'oak.history.openFileChange', title: 'Open Changes', arguments: [node] };
      return item;
    }
    if (node instanceof MoreNode) {
      const item = new vscode.TreeItem('Load More...', vscode.TreeItemCollapsibleState.None);
      item.iconPath = new vscode.ThemeIcon('ellipsis');
      item.command = { command: 'oak.history.loadMore', title: 'Load More', arguments: [node] };
      return item;
    }
    const item = new vscode.TreeItem(node.message, vscode.TreeItemCollapsibleState.None);
    item.iconPath = new vscode.ThemeIcon('info');
    return item;
  }

  async getChildren(node?: HistoryNode): Promise<HistoryNode[]> {
    if (!node) {
      let repos = this.model.repositories;
      if (this.fileFilter) {
        repos = repos.filter((r) => r.contains(this.fileFilter!));
      }
      if (repos.length === 0) {
        return [];
      }
      if (repos.length === 1) {
        return this.commitsFor(repos[0]);
      }
      return repos.map((r) => new RepoNode(r));
    }
    if (node instanceof RepoNode) {
      return this.commitsFor(node.repo);
    }
    if (node instanceof CommitNode) {
      return this.filesFor(node);
    }
    return [];
  }

  private async commitsFor(repo: Repository): Promise<HistoryNode[]> {
    const limit = this.limit(repo);
    const paths = this.fileFilter ? [repo.relativePath(this.fileFilter)] : undefined;
    try {
      // Ask for one extra so the last shown commit knows its parent.
      const entries = await repo.log({ limit: limit + 1, paths });
      if (entries.length === 0) {
        return [new MessageNode(paths ? 'No commits touch this file' : 'No commits yet')];
      }
      const shown = entries.slice(0, limit);
      const nodes: HistoryNode[] = shown.map(
        (e, i) => new CommitNode(repo, e, paths ? undefined : entries[i + 1]?.hash, e.hash === repo.head),
      );
      if (entries.length > limit) {
        nodes.push(new MoreNode(repo));
      }
      return nodes;
    } catch (err) {
      return [new MessageNode(`Failed to load history: ${err instanceof Error ? err.message : err}`)];
    }
  }

  private async filesFor(node: CommitNode): Promise<HistoryNode[]> {
    try {
      const parent = node.parentHash ?? (await this.resolveParent(node));
      if (!parent) {
        const files = await node.repo.treeFiles(node.hash);
        return files.map((p) => new CommitFileNode(new CommitNode(node.repo, node.entry, undefined, node.isHead), { path: p, status: 'added' }));
      }
      const resolved = parent === node.parentHash ? node : new CommitNode(node.repo, node.entry, parent, node.isHead);
      const files = await node.repo.diffFiles(parent, node.hash);
      let list = files.map((f) => new CommitFileNode(resolved, f));
      if (this.fileFilter) {
        const rel = node.repo.relativePath(this.fileFilter);
        list = list.sort((a, b) => Number(b.file.path === rel) - Number(a.file.path === rel));
      }
      return list.length > 0 ? list : [new MessageNode('No file changes')];
    } catch (err) {
      return [new MessageNode(`Failed to load changes: ${err instanceof Error ? err.message : err}`)];
    }
  }

  /**
   * In a file-filtered listing the next row is the previous commit that
   * touched the file, not the parent; look the real parent up in the
   * unfiltered log instead.
   */
  private async resolveParent(node: CommitNode): Promise<string | undefined> {
    const entries = await node.repo.log({ limit: 2000 });
    const idx = entries.findIndex((e) => e.hash === node.hash);
    return idx >= 0 ? entries[idx + 1]?.hash : undefined;
  }

  getParent(): undefined {
    return undefined;
  }

  dispose(): void {
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
    }
    vscode.Disposable.from(...this.disposables).dispose();
  }
}

/** Open the diff for one file of a commit. */
export async function openCommitFileChange(node: CommitFileNode): Promise<void> {
  const { commit, file } = node;
  const title = `${path.basename(file.path)} (${shortHash(commit.parentHash) || 'empty'} ↔ ${shortHash(commit.hash)})`;
  const right = toOakUri(node.uri, commit.hash);
  if (file.status === 'added' || !commit.parentHash) {
    await vscode.commands.executeCommand('vscode.open', right, { preview: true }, `${path.basename(file.path)} (added in ${shortHash(commit.hash)})`);
    return;
  }
  const leftPath = file.old_path ? commit.repo.absolute(file.old_path) : node.uri;
  const left = toOakUri(leftPath, commit.parentHash);
  if (file.status === 'deleted') {
    await vscode.commands.executeCommand('vscode.open', left, { preview: true }, `${path.basename(file.path)} (deleted in ${shortHash(commit.hash)})`);
    return;
  }
  await vscode.commands.executeCommand('vscode.diff', left, right, title, { preview: true });
}

/** Open every file of a commit in the multi-file diff editor. */
export async function openCommitChanges(provider: HistoryProvider, node: CommitNode): Promise<void> {
  const children = (await provider.getChildren(node)).filter((c): c is CommitFileNode => c instanceof CommitFileNode);
  if (children.length === 0) {
    void vscode.window.showInformationMessage('This commit has no file changes.');
    return;
  }
  const parent = children[0].commit.parentHash;
  const resources = children.map((c) => {
    const left = parent && c.file.status !== 'added' ? toOakUri(c.file.old_path ? c.commit.repo.absolute(c.file.old_path) : c.uri, parent) : undefined;
    const right = c.file.status === 'deleted' ? undefined : toOakUri(c.uri, c.commit.hash);
    return [c.uri, left, right];
  });
  await vscode.commands.executeCommand('vscode.changes', `${commitSubject(node.entry)} (${shortHash(node.hash)})`, resources);
}
