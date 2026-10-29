// End-to-end checks run inside a real VS Code against a fresh `oak init`
// repository (see ../runTest.ts). A tiny sequential harness keeps the test
// dependencies to @vscode/test-electron alone.

import { strict as assert } from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import type { OakExtensionApi } from '../../../src/extension';
import { CommitFileNode, CommitNode, HistoryNode } from '../../../src/history';
import type { Repository } from '../../../src/repository';
import { HEAD_REF, toOakUri } from '../../../src/uri';

type Test = { name: string; fn: () => Promise<void> };
const tests: Test[] = [];
const test = (name: string, fn: () => Promise<void>) => tests.push({ name, fn });

const workspace = process.env.OAK_TEST_WORKSPACE!;
let api: OakExtensionApi;
let repo: Repository;

const file = (rel: string) => path.join(workspace, ...rel.split('/'));
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function until<T>(what: string, fn: () => T | undefined | false | Promise<T | undefined | false>, timeoutMs = 15000): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v) {
      return v;
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await sleep(100);
  }
}

const workingPaths = () => repo.workingResources.map((r) => `${r.letter} ${repo.relativePath(r.resourceUri.fsPath)}`).sort();

test('activates and discovers the repository', async () => {
  const ext = vscode.extensions.all.find((e) => e.packageJSON.name === 'oak-vcs');
  assert.ok(ext, 'extension is installed');
  api = (await ext!.activate()) as OakExtensionApi;
  assert.ok(api, 'extension returned its API');
  repo = await until('repository', () => api.model.repositories[0]);
  assert.equal(fs.realpathSync(repo.root), fs.realpathSync(workspace));
  await repo.refresh();
  assert.ok(repo.head, 'HEAD is known');
  assert.deepEqual(workingPaths(), []);
});

test('working-tree edits show up as Changes (via the file watcher)', async () => {
  fs.appendFileSync(file('hello.txt'), 'line three\n');
  fs.writeFileSync(file('new.txt'), 'brand new\n');
  fs.rmSync(file('src/main.ts'));
  await until('three changes', () => workingPaths().length === 3);
  assert.deepEqual(workingPaths(), ['A new.txt', 'D src/main.ts', 'M hello.txt']);
  assert.equal(repo.sourceControl.count, 3);
});

test('file decorations badge changed files', async () => {
  // Decorations are pushed through the workbench; read them via the provider API indirectly
  // by checking the resource letters VS Code renders in the SCM view.
  const modified = repo.workingResources.find((r) => r.resourceUri.fsPath.endsWith('hello.txt'))!;
  assert.equal(modified.letter, 'M');
  assert.equal(modified.decorations.strikeThrough, false);
  const deleted = repo.workingResources.find((r) => r.status === 'deleted')!;
  assert.equal(deleted.decorations.strikeThrough, true);
});

test('quick diff original resource serves HEAD content', async () => {
  const uri = vscode.Uri.file(file('hello.txt'));
  const original = repo.provideOriginalResource(uri);
  assert.ok(original, 'modified file has an original');
  const doc = await vscode.workspace.openTextDocument(original!);
  assert.equal(doc.getText(), 'line one\nline two\n');
  assert.equal(repo.provideOriginalResource(vscode.Uri.file(file('new.txt'))), undefined, 'added file has no original');
  const deletedDoc = await vscode.workspace.openTextDocument(toOakUri(vscode.Uri.file(file('src/main.ts')), HEAD_REF));
  assert.equal(deletedDoc.getText(), 'export const x = 1;\n');
});

test('opening a resource opens a diff editor', async () => {
  const modified = repo.workingResources.find((r) => r.resourceUri.fsPath.endsWith('hello.txt'))!;
  await vscode.commands.executeCommand('oak.openResource', modified);
  await until('diff editor', () => vscode.window.tabGroups.activeTabGroup.activeTab?.input instanceof vscode.TabInputTextDiff);
  const input = vscode.window.tabGroups.activeTabGroup.activeTab!.input as vscode.TabInputTextDiff;
  assert.equal(input.original.scheme, 'oak');
  assert.ok(input.modified.fsPath.endsWith('hello.txt'));
  await vscode.commands.executeCommand('workbench.action.closeAllEditors');
});

test('checkpointing selected paths commits only those', async () => {
  const before = repo.head;
  const result = await repo.commit(['new.txt']);
  assert.ok(result.committed);
  assert.notEqual(repo.head, before);
  assert.deepEqual(workingPaths(), ['D src/main.ts', 'M hello.txt']);
});

test('discarding restores modified and deleted files', async () => {
  await repo.discard(['hello.txt', 'src/main.ts']);
  assert.equal(fs.readFileSync(file('hello.txt'), 'utf8'), 'line one\nline two\n');
  assert.ok(fs.existsSync(file('src/main.ts')));
  assert.deepEqual(workingPaths(), []);
});

test('discarding an added file deletes it', async () => {
  fs.writeFileSync(file('scratch.txt'), 'tmp\n');
  await repo.refresh();
  assert.deepEqual(workingPaths(), ['A scratch.txt']);
  await repo.discard(['scratch.txt']);
  assert.ok(!fs.existsSync(file('scratch.txt')));
});

test('discard all resets the working tree', async () => {
  fs.appendFileSync(file('hello.txt'), 'x\n');
  fs.writeFileSync(file('other.txt'), 'y\n');
  await repo.refresh();
  assert.equal(repo.workingResources.length, 2);
  await repo.discardAll();
  assert.deepEqual(workingPaths(), []);
  assert.ok(!fs.existsSync(file('other.txt')));
});

test('the input box carries the branch description into the checkpoint', async () => {
  fs.appendFileSync(file('hello.txt'), 'described\n');
  await repo.refresh();
  repo.sourceControl.inputBox.value = 'Add a described line\n\nMore detail.';
  await vscode.commands.executeCommand('oak.commit', repo.sourceControl);
  await repo.refresh();
  assert.equal(repo.status?.branch_description?.trim(), 'Add a described line\n\nMore detail.');
  assert.deepEqual(workingPaths(), []);
  assert.equal(repo.sourceControl.inputBox.value, 'Add a described line\n\nMore detail.', 'input box keeps showing the description');
});

test('history lists commits and their files', async () => {
  const entries = await repo.log({ limit: 10 });
  assert.equal(entries.length, 3);
  assert.equal(entries[0].hash, repo.head);
  const files = await repo.diffFiles(entries[1].hash, entries[0].hash);
  assert.deepEqual(
    files.map((f) => `${f.status} ${f.path}`),
    ['modified hello.txt'],
  );
});

test('history view nodes resolve parents and root commits', async () => {
  const { HistoryProvider } = await import('../../../src/history');
  const view = new HistoryProvider(api.model);
  try {
    const roots = (await view.getChildren()) as HistoryNode[];
    const commits = roots.filter((n): n is CommitNode => n instanceof CommitNode);
    assert.equal(commits.length, 3);
    assert.ok(commits[0].isHead);
    const headFiles = (await view.getChildren(commits[0])).filter((n): n is CommitFileNode => n instanceof CommitFileNode);
    assert.deepEqual(
      headFiles.map((f) => f.file.path),
      ['hello.txt'],
    );
    const rootFiles = (await view.getChildren(commits[2])).filter((n): n is CommitFileNode => n instanceof CommitFileNode);
    assert.deepEqual(rootFiles.map((f) => f.file.path).sort(), ['hello.txt', 'src/main.ts']);
    // Content at an old revision.
    const old = await vscode.workspace.openTextDocument(toOakUri(vscode.Uri.file(file('hello.txt')), commits[2].hash));
    assert.equal(old.getText(), 'line one\nline two\n');
    // File-filtered history.
    view.setFileFilter(file('src/main.ts'));
    const filtered = (await view.getChildren()).filter((n): n is CommitNode => n instanceof CommitNode);
    assert.equal(filtered.length, 1);
  } finally {
    view.dispose();
  }
});

test('branches: create, list, rename, switch', async () => {
  const original = repo.branch!;
  await repo.createBranch('it-feature');
  assert.equal(repo.branch, 'it-feature');
  const names = (await repo.branches()).map((b) => b.name);
  assert.ok(names.includes('it-feature'));
  assert.ok(names.includes(original));
  await repo.renameBranch('it-feature', 'it-feature-2');
  await repo.refresh();
  assert.equal(repo.branch, 'it-feature-2');
  await repo.switchBranch(original);
  assert.equal(repo.branch, original);
});

test('restore a file from an older commit', async () => {
  const entries = await repo.log({ limit: 10 });
  const rootCommit = entries[entries.length - 1].hash;
  await repo.restoreFrom(['hello.txt'], rootCommit);
  assert.equal(fs.readFileSync(file('hello.txt'), 'utf8'), 'line one\nline two\n');
  assert.deepEqual(workingPaths(), ['M hello.txt']);
  await repo.discardAll();
});

test('push button sits in the SCM title bar and Checkpoint & Push on Changes', async () => {
  const pkg = vscode.extensions.all.find((e) => e.packageJSON.name === 'oak-vcs')!.packageJSON;
  const title = pkg.contributes.menus['scm/title'] as { command: string; group: string }[];
  assert.ok(title.some((m) => m.command === 'oak.push' && m.group.startsWith('navigation')));
  const group = pkg.contributes.menus['scm/resourceGroup/context'] as { command: string; group: string }[];
  assert.ok(group.some((m) => m.command === 'oak.commitAndPush' && m.group.startsWith('inline')));
  // Unlinked repo: no push indicator even with local commits.
  assert.equal(repo.needsPush, false);
  assert.ok(repo.pushStateKnown, 'agent state was read');
});

test('status bar shows the branch', async () => {
  const cmds = repo.sourceControl.statusBarCommands ?? [];
  assert.ok(cmds[0]?.title.includes(repo.branch!), `branch in ${cmds[0]?.title}`);
  // Unlinked repository → the publish action.
  assert.ok(cmds.some((c) => c.command === 'oak.publish'));
});

test('commands are registered', async () => {
  const all = new Set(await vscode.commands.getCommands(true));
  const pkg = vscode.extensions.all.find((e) => e.packageJSON.name === 'oak-vcs')!.packageJSON;
  for (const c of pkg.contributes.commands as { command: string }[]) {
    assert.ok(all.has(c.command), `${c.command} registered`);
  }
});

export async function run(): Promise<void> {
  let failed = 0;
  let passed = 0;
  for (const t of tests) {
    const started = Date.now();
    try {
      await t.fn();
      passed++;
      console.log(`  ✓ ${t.name} (${Date.now() - started}ms)`);
    } catch (err) {
      failed++;
      console.error(`  ✗ ${t.name}\n    ${err instanceof Error ? err.stack ?? err.message : err}`);
      // Later tests depend on earlier state; stop at the first failure.
      break;
    }
  }
  console.log(`${passed}/${tests.length} passed${failed ? ' (stopped at first failure)' : ''}`);
  if (failed) {
    throw new Error(`${failed} integration test(s) failed`);
  }
}
