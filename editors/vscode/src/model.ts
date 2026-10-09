import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import { Oak } from './oak';
import { Repository } from './repository';

/** Directory names never scanned for nested repositories. */
const SKIP_DIRS = new Set(['node_modules', 'target', '.git', '.oak', 'out', 'dist', 'build', '.venv']);

/** Finds Oak repositories in the workspace and owns one `Repository` each. */
export class Model implements vscode.Disposable {
  private readonly repos = new Map<string, Repository>();
  private readonly disposables: vscode.Disposable[] = [];
  private readonly _onDidOpenRepository = new vscode.EventEmitter<Repository>();
  readonly onDidOpenRepository = this._onDidOpenRepository.event;
  private readonly _onDidCloseRepository = new vscode.EventEmitter<Repository>();
  readonly onDidCloseRepository = this._onDidCloseRepository.event;
  private readonly _onDidChangeRepositoryState = new vscode.EventEmitter<Repository>();
  /** Any repository refreshed. */
  readonly onDidChangeRepositoryState = this._onDidChangeRepositoryState.event;

  constructor(
    readonly oak: Oak,
    private readonly output: vscode.OutputChannel,
  ) {
    this.disposables.push(this._onDidOpenRepository, this._onDidCloseRepository, this._onDidChangeRepositoryState);
    this.disposables.push(
      vscode.workspace.onDidChangeWorkspaceFolders((e) => {
        for (const folder of e.removed) {
          for (const repo of this.repositories) {
            if (repo.contains(folder.uri.fsPath) || isInside(folder.uri.fsPath, repo.root)) {
              if (!vscode.workspace.workspaceFolders?.some((f) => repo.contains(f.uri.fsPath) || isInside(f.uri.fsPath, repo.root))) {
                this.close(repo);
              }
            }
          }
        }
        void this.scanFolders(e.added);
      }),
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor?.document.uri.scheme === 'file') {
          void this.openFor(editor.document.uri.fsPath);
        }
      }),
    );
  }

  get repositories(): Repository[] {
    return [...this.repos.values()];
  }

  async scanWorkspace(): Promise<void> {
    await this.scanFolders(vscode.workspace.workspaceFolders ?? []);
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.scheme === 'file') {
        await this.openFor(editor.document.uri.fsPath);
      }
    }
  }

  private async scanFolders(folders: readonly vscode.WorkspaceFolder[]): Promise<void> {
    for (const folder of folders) {
      const config = vscode.workspace.getConfiguration('oak', folder.uri);
      const depth = config.get<number>('repositoryScanMaxDepth', 1);
      const root = findRepositoryRoot(folder.uri.fsPath);
      if (root) {
        await this.open(root);
      }
      for (const nested of await scanForRepositories(folder.uri.fsPath, depth)) {
        await this.open(nested);
      }
    }
  }

  /** Open the repository containing `fsPath`, if any. */
  async openFor(fsPath: string): Promise<Repository | undefined> {
    const existing = this.getRepository(fsPath);
    if (existing) {
      return existing;
    }
    const root = findRepositoryRoot(path.dirname(fsPath));
    return root ? this.open(root) : undefined;
  }

  async open(root: string): Promise<Repository> {
    const key = normalizeKey(root);
    const existing = this.repos.get(key);
    if (existing) {
      return existing;
    }
    this.output.appendLine(`Opening Oak repository ${root}`);
    const repo = new Repository(root, this.oak, this.output);
    this.repos.set(key, repo);
    repo.onDidChangeState(() => this._onDidChangeRepositoryState.fire(repo), undefined, this.disposables);
    await vscode.commands.executeCommand('setContext', 'oak.hasRepository', true);
    this._onDidOpenRepository.fire(repo);
    await repo.refresh();
    return repo;
  }

  close(repo: Repository): void {
    this.repos.delete(normalizeKey(repo.root));
    repo.dispose();
    this._onDidCloseRepository.fire(repo);
    void vscode.commands.executeCommand('setContext', 'oak.hasRepository', this.repos.size > 0);
  }

  /** The innermost open repository containing `fsPath`. */
  getRepository(target: string | vscode.Uri | vscode.SourceControl | undefined): Repository | undefined {
    if (!target) {
      return undefined;
    }
    if (typeof target !== 'string' && 'inputBox' in target) {
      return this.repositories.find((r) => r.sourceControl === target);
    }
    let fsPath: string;
    if (typeof target === 'string') {
      fsPath = target;
    } else if (target.scheme === 'oak') {
      try {
        fsPath = JSON.parse(target.query).path;
      } catch {
        return undefined;
      }
    } else {
      fsPath = target.fsPath;
    }
    let best: Repository | undefined;
    for (const repo of this.repos.values()) {
      if (repo.contains(fsPath) && (!best || repo.root.length > best.root.length)) {
        best = repo;
      }
    }
    return best;
  }

  dispose(): void {
    for (const repo of this.repos.values()) {
      repo.dispose();
    }
    this.repos.clear();
    vscode.Disposable.from(...this.disposables).dispose();
  }
}

function normalizeKey(p: string): string {
  const resolved = path.resolve(p);
  return process.platform === 'win32' || process.platform === 'darwin' ? resolved.toLowerCase() : resolved;
}

function isInside(child: string, parent: string): boolean {
  const rel = path.relative(parent, child);
  return !rel.startsWith('..') && !path.isAbsolute(rel);
}

/** Walk up from `start` looking for a directory containing `.oak/oak.db`. */
export function findRepositoryRoot(start: string): string | undefined {
  let dir = path.resolve(start);
  for (;;) {
    if (isOakRoot(dir)) {
      return dir;
    }
    const parent = path.dirname(dir);
    if (parent === dir) {
      return undefined;
    }
    dir = parent;
  }
}

function isOakRoot(dir: string): boolean {
  try {
    const marker = path.join(dir, '.oak');
    if (!fs.statSync(marker).isDirectory()) {
      return false;
    }
    // `~/.oak` holds global state (mounts, credentials), not a repository.
    return fs.existsSync(path.join(marker, 'oak.db')) || fs.existsSync(path.join(marker, 'HEAD'));
  } catch {
    return false;
  }
}

async function scanForRepositories(dir: string, depth: number): Promise<string[]> {
  if (depth <= 0) {
    return [];
  }
  let entries: fs.Dirent[];
  try {
    entries = await fs.promises.readdir(dir, { withFileTypes: true });
  } catch {
    return [];
  }
  const found: string[] = [];
  for (const entry of entries) {
    if (!entry.isDirectory() || SKIP_DIRS.has(entry.name) || entry.name.startsWith('.')) {
      continue;
    }
    const child = path.join(dir, entry.name);
    if (isOakRoot(child)) {
      found.push(child);
    }
    found.push(...(await scanForRepositories(child, depth - 1)));
  }
  return found;
}
