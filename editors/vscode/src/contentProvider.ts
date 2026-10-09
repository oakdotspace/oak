import * as vscode from 'vscode';
import { Model } from './model';
import { Repository } from './repository';
import { HEAD_REF, OAK_SCHEME, fromOakUri } from './uri';

const CACHE_LIMIT = 200;

/**
 * Serves `oak:` URIs: a file's committed content at a revision. Content at a
 * full commit hash is immutable, so it is cached; `HEAD` URIs resolve through
 * the repository's current head and are re-fired when HEAD moves so quick diff
 * and open diff editors update after a checkpoint.
 */
export class OakContentProvider implements vscode.TextDocumentContentProvider, vscode.Disposable {
  private readonly _onDidChange = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this._onDidChange.event;
  private readonly cache = new Map<string, string>();
  private readonly disposables: vscode.Disposable[] = [];

  constructor(private readonly model: Model) {
    this.disposables.push(
      this._onDidChange,
      vscode.workspace.registerTextDocumentContentProvider(OAK_SCHEME, this),
      model.onDidOpenRepository((repo) => this.watch(repo)),
    );
    for (const repo of model.repositories) {
      this.watch(repo);
    }
  }

  private watch(repo: Repository): void {
    repo.onDidChangeHead(
      () => {
        for (const doc of vscode.workspace.textDocuments) {
          if (doc.uri.scheme !== OAK_SCHEME) {
            continue;
          }
          try {
            const params = fromOakUri(doc.uri);
            if (params.ref === HEAD_REF && repo.contains(params.path)) {
              this._onDidChange.fire(doc.uri);
            }
          } catch {
            // not ours
          }
        }
      },
      undefined,
      this.disposables,
    );
  }

  async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    const { path: fsPath, ref } = fromOakUri(uri);
    const repo = this.model.getRepository(fsPath) ?? (await this.model.openFor(fsPath));
    if (!repo) {
      return '';
    }
    const commit = ref === HEAD_REF ? repo.head : ref;
    if (!commit) {
      return ''; // no commits yet
    }
    const rel = repo.relativePath(fsPath);
    const key = `${commit}:${rel}`;
    const cached = this.cache.get(key);
    if (cached !== undefined) {
      // refresh LRU position
      this.cache.delete(key);
      this.cache.set(key, cached);
      return cached;
    }
    const bytes = await repo.fileContentAt(rel, commit);
    const text = bytes ? bytes.toString('utf8') : '';
    this.cache.set(key, text);
    if (this.cache.size > CACHE_LIMIT) {
      const oldest = this.cache.keys().next().value;
      if (oldest !== undefined) {
        this.cache.delete(oldest);
      }
    }
    return text;
  }

  dispose(): void {
    vscode.Disposable.from(...this.disposables).dispose();
  }
}
