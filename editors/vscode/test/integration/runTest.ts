// Launches a real VS Code with the extension under development against a
// throwaway Oak repository. Set VSCODE_EXECUTABLE to use an installed VS Code
// instead of downloading one.
import * as cp from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { runTests } from '@vscode/test-electron';

async function main(): Promise<void> {
  const extensionDevelopmentPath = path.resolve(__dirname, '../../..');
  const extensionTestsPath = path.resolve(__dirname, './suite/index');
  const workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'oak-vscode-it-'));
  const oak = process.env.OAK_BIN ?? 'oak';
  const run = (args: string[]) => cp.execFileSync(oak, args, { cwd: workspace, env: { ...process.env, OAK_NO_UPDATE_CHECK: '1' }, stdio: 'pipe' });

  run(['init']);
  fs.writeFileSync(path.join(workspace, 'hello.txt'), 'line one\nline two\n');
  fs.mkdirSync(path.join(workspace, 'src'));
  fs.writeFileSync(path.join(workspace, 'src', 'main.ts'), 'export const x = 1;\n');
  run(['commit']);

  const vscodeExecutablePath = process.env.VSCODE_EXECUTABLE;
  try {
    await runTests({
      vscodeExecutablePath,
      extensionDevelopmentPath,
      extensionTestsPath,
      launchArgs: [workspace, '--disable-extensions', '--disable-workspace-trust', '--skip-welcome', '--skip-release-notes'],
      extensionTestsEnv: { OAK_TEST_WORKSPACE: workspace },
    });
  } finally {
    fs.rmSync(workspace, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
