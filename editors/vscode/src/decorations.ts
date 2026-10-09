import * as vscode from 'vscode';
import { Model } from './model';
import { statusLabel, statusLetter } from './parse';
import { Repository } from './repository';

const COLORS: Record<string, string> = {
  added: 'oakDecoration.addedResourceForeground',
  modified: 'oakDecoration.modifiedResourceForeground',
  deleted: 'oakDecoration.deletedResourceForeground',
  renamed: 'oakDecoration.renamedResourceForeground',
  conflicted: 'oakDecoration.conflictingResourceForeground',
};

/** Explorer / tab / SCM-view badges ("M", "A", ...) for changed files. */
export class OakDecorationProvider implements vscode.FileDecorationProvider, vscode.Disposable {
  private readonly _onDidChange = new vscode.EventEmitter<vscode.Uri[]>();
  readonly onDidChangeFileDecorations = this._onDidChange.event;
  /** uri string -> decoration, per repository. */
  private readonly perRepo = new Map<Repository, Map<string, vscode.FileDecoration>>();
  private readonly disposables: vscode.Disposable[] = [];

  constructor(private readonly model: Model) {
    this.disposables.push(
      this._onDidChange,
      vscode.window.registerFileDecorationProvider(this),
      model.onDidChangeRepositoryState((repo) => this.update(repo)),
      model.onDidCloseRepository((repo) => {
        const old = this.perRepo.get(repo);
        this.perRepo.delete(repo);
        if (old) {
          this._onDidChange.fire([...old.keys()].map((k) => vscode.Uri.parse(k)));
        }
      }),
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (e.affectsConfiguration('oak.decorations.enabled')) {
          for (const repo of model.repositories) {
            this.update(repo);
          }
        }
      }),
    );
    for (const repo of model.repositories) {
      this.update(repo);
    }
  }

  private update(repo: Repository): void {
    const enabled = vscode.workspace.getConfiguration('oak').get<boolean>('decorations.enabled', true);
    const next = new Map<string, vscode.FileDecoration>();
    if (enabled) {
      for (const r of [...repo.conflictResources, ...repo.workingResources]) {
        const status = r.status;
        next.set(r.resourceUri.toString(), {
          badge: statusLetter(status),
          tooltip: statusLabel(status),
          color: COLORS[status] ? new vscode.ThemeColor(COLORS[status]) : undefined,
          propagate: status !== 'deleted',
        });
      }
    }
    const prev = this.perRepo.get(repo) ?? new Map<string, vscode.FileDecoration>();
    this.perRepo.set(repo, next);
    const changed = new Set<string>();
    for (const [k, v] of next) {
      const old = prev.get(k);
      if (!old || old.badge !== v.badge) {
        changed.add(k);
      }
    }
    for (const k of prev.keys()) {
      if (!next.has(k)) {
        changed.add(k);
      }
    }
    if (changed.size > 0) {
      this._onDidChange.fire([...changed].map((k) => vscode.Uri.parse(k)));
    }
  }

  provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== 'file') {
      return undefined;
    }
    const repo = this.model.getRepository(uri);
    return repo ? this.perRepo.get(repo)?.get(uri.toString()) : undefined;
  }

  dispose(): void {
    vscode.Disposable.from(...this.disposables).dispose();
  }
}
