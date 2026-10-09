import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';
import { CommandCenter } from './commands';
import { OakContentProvider } from './contentProvider';
import { OakDecorationProvider } from './decorations';
import { HistoryProvider } from './history';
import { Model } from './model';
import { Oak } from './oak';

/** Public API other extensions (and the integration tests) can use. */
export interface OakExtensionApi {
  model: Model;
  oak: Oak;
}

export async function activate(context: vscode.ExtensionContext): Promise<OakExtensionApi | undefined> {
  const output = vscode.window.createOutputChannel('Oak');
  context.subscriptions.push(output);

  const config = vscode.workspace.getConfiguration('oak');
  if (!config.get<boolean>('enabled', true)) {
    output.appendLine('Oak is disabled (oak.enabled = false).');
    return undefined;
  }

  const oakPath = resolveOakPath(config.get<string | null>('path') ?? undefined);
  const timestamp = () => new Date().toISOString().slice(11, 23);
  const oak = new Oak(oakPath, (line) => output.appendLine(`${timestamp()} ${line}`));

  try {
    const version = await oak.version();
    output.appendLine(`Using oak ${version} at ${oakPath}`);
  } catch (err) {
    output.appendLine(`Could not run oak at ${oakPath}: ${err instanceof Error ? err.message : err}`);
    await vscode.commands.executeCommand('setContext', 'oak.missing', true);
    void vscode.window
      .showWarningMessage(
        'Oak: the `oak` CLI was not found. Install it from oak.space or set "oak.path".',
        'Install Oak',
        'Open Settings',
      )
      .then((choice) => {
        if (choice === 'Install Oak') {
          void vscode.env.openExternal(vscode.Uri.parse('https://oak.space/docs'));
        } else if (choice === 'Open Settings') {
          void vscode.commands.executeCommand('workbench.action.openSettings', 'oak.path');
        }
      });
  }

  const model = new Model(oak, output);
  const contentProvider = new OakContentProvider(model);
  const decorations = new OakDecorationProvider(model);
  const history = new HistoryProvider(model);
  const commands = new CommandCenter(model, oak, history, output);
  context.subscriptions.push(model, contentProvider, decorations, history, commands);

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('oak.path') || e.affectsConfiguration('oak.enabled')) {
        void vscode.window
          .showInformationMessage('Reload the window for the Oak setting change to take effect.', 'Reload')
          .then((c) => c && vscode.commands.executeCommand('workbench.action.reloadWindow'));
      }
    }),
  );

  // Context keys driving menus/when-clauses.
  const updateContext = () => {
    const repos = model.repositories;
    void vscode.commands.executeCommand('setContext', 'oak.hasRepository', repos.length > 0);
    void vscode.commands.executeCommand('setContext', 'oak.conflictInProgress', repos.some((r) => r.conflict?.in_progress));
    void vscode.commands.executeCommand('setContext', 'oak.anyUnlinked', repos.some((r) => r.status && !r.isLinked));
  };
  context.subscriptions.push(model.onDidChangeRepositoryState(updateContext), model.onDidCloseRepository(updateContext));

  await model.scanWorkspace();
  updateContext();
  await vscode.commands.executeCommand('setContext', 'oak.state', 'initialized');

  return { model, oak };
}

export function deactivate(): void {
  // Disposables registered on the context are cleaned up by VS Code.
}

/**
 * `oak.path` if set; otherwise `oak` on PATH, falling back to the default
 * install locations (VS Code launched from the Dock does not inherit a login
 * shell's PATH on macOS).
 */
function resolveOakPath(configured: string | undefined): string {
  if (configured && configured.trim()) {
    return configured.replace(/^~(?=$|[\\/])/, os.homedir());
  }
  const exe = process.platform === 'win32' ? 'oak.exe' : 'oak';
  const dirs = (process.env.PATH ?? '').split(path.delimiter).filter(Boolean);
  dirs.push(
    path.join(os.homedir(), '.cargo', 'bin'),
    path.join(os.homedir(), '.local', 'bin'),
    path.join(os.homedir(), '.oak', 'bin'),
    '/opt/homebrew/bin',
    '/usr/local/bin',
  );
  for (const dir of dirs) {
    const candidate = path.join(dir, exe);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {
      // keep looking
    }
  }
  return exe;
}
