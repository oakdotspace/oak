#!/usr/bin/env node
// Hermetic tests of the shared native workflow entry point. Node is supplied by
// the runner's Bun/node compatibility entry point; no package install is needed.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, readFile, chmod, readdir, rm } from 'node:fs/promises';
import { existsSync, readFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const execute = promisify(execFile);
const repo = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const tools = ['cargo-nextest', 'cargo-llvm-cov'];
// Keep the production API asset and its independently verified release digest
// bound together; URL overrides below are only for the hermetic scenarios.
const bootstrap = readFileSync(join(repo, 'scripts/bootstrap-ci-tool.sh'), 'utf8');
assert(bootstrap.includes('url=https://api.github.com/repos/taiki-e/cargo-llvm-cov/releases/assets/547403171'));
assert(bootstrap.includes('digest=b3f68e625481fed9b16444174f3fa5ebcdbde4a1878803a35eabe2dcefcdc41a'));
assert(bootstrap.includes('url=https://get.nexte.st/latest/linux'));
const payload = Buffer.from('#!/bin/sh\n#' + Array.from({ length: 512 }, (_, n) =>
  createHash('sha256').update(String(n)).digest('hex')).join('') + '\nprintf "fixture-tool\\n"\n');
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";

async function scenario(tool, behavior, { existing, direct = false } = {}) {
  const root = await mkdtemp(join(tmpdir(), 'oak-ci-bootstrap-'));
  let server;
  const sockets = new Set();
  try {
    const cargo = join(root, 'cargo');
    const bin = join(cargo, 'bin');
    const source = join(root, 'source');
    const target = join(bin, tool);
    await mkdir(bin, { recursive: true });
    await mkdir(source);
    await writeFile(join(cargo, 'env'), ':\n');
    await writeFile(join(bin, 'rustup'), '#!/bin/sh\nprintf "%s\\n" "$*" >> "$OAK_TEST_RUSTUP_LOG"\n', { mode: 0o755 });
    await writeFile(join(source, tool), payload, { mode: 0o755 });
    await execute('tar', ['-czf', join(root, 'fixture.tar.gz'), '-C', source, tool]);
    const archive = await readFile(join(root, 'fixture.tar.gz'));
    if (existing !== undefined) {
      await writeFile(target, existing, { mode: 0o755 });
      await chmod(target, 0o755);
    }
    const digest = behavior === 'digest-mismatch' ? '0'.repeat(64)
      : createHash('sha256').update(behavior === 'corrupt' ? archive.subarray(0, archive.length / 2) : archive).digest('hex');
    const requests = [];
    // Raw HTTP over a local TCP socket makes an intentionally truncated body
    // deterministic under both Node and Bun (HTTP wrappers may buffer it).
    server = createServer(socket => {
      sockets.add(socket);
      socket.on('close', () => sockets.delete(socket));
      socket.on('error', () => {});
      let request = '';
      let answered = false;
      socket.on('data', chunk => {
        request += chunk.toString();
        if (answered || !request.includes('\r\n\r\n')) return;
        answered = true;
        assert(request.toLowerCase().includes('accept: application/octet-stream'), 'asset API binary representation requested');
        requests.push({ path: request.split(' ')[1], prior: existsSync(target) ? readFileSync(target) : null });
        const attempt = requests.length;
        if (behavior === 'permanent' || (behavior === 'transient' && attempt === 1)) {
          socket.end('HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n');
          return;
        }
        const body = behavior === 'metadata-json' ? Buffer.from('{"name":"cargo-llvm-cov"}')
          : behavior === 'corrupt' ? archive.subarray(0, archive.length / 2) : archive;
        const truncated = behavior === 'truncated-always' || (behavior === 'truncated' && attempt === 1);
        const header = Buffer.from(`HTTP/1.1 200 OK\r\nContent-Length: ${body.length}\r\nConnection: close\r\n\r\n`);
        socket.end(Buffer.concat([header, truncated ? body.subarray(0, body.length / 2) : body]));
      });
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const url = `http://127.0.0.1:${server.address().port}/archive.tar.gz`;
    // The workflow invokes this same exported script. Only its cargo directory
    // and archive URL are replaced; guards, rustup, curl and tar are unchanged.
    const command = direct
      ? `bash scripts/install-ci-tool.sh ${quote(tool)} ${quote(url)} ${quote(bin)} ${quote(digest)}`
      : `bash scripts/bootstrap-ci-tool.sh ${quote(tool)} ${quote(cargo)} ${quote(url)} ${quote(digest)}`;
    let result;
    const started = Date.now();
    try {
      result = { code: 0, ...await execute('/bin/bash', ['-e', '-o', 'pipefail', '-c', command], {
        cwd: repo, timeout: 30000,
        env: { ...process.env, PATH: bin + ':/usr/bin:/bin:/usr/sbin:/sbin', NO_PROXY: '127.0.0.1,localhost', no_proxy: '127.0.0.1,localhost', OAK_TEST_RUSTUP_LOG: join(root, 'rustup.log') },
      }) };
    } catch (error) {
      assert(!error.killed, 'bootstrap exceeded the fixture deadline');
      result = { code: error.code, stdout: error.stdout, stderr: error.stderr };
    }
    const installed = existsSync(target) ? await readFile(target) : null;
    const leftovers = (await readdir(bin)).filter(name => name !== tool && name !== 'rustup');
    const rustup = existsSync(join(root, 'rustup.log')) ? await readFile(join(root, 'rustup.log'), 'utf8') : '';
    return { ...result, installed, requests, leftovers, rustup, elapsed: Date.now() - started };
  } finally {
    if (server) {
      for (const socket of sockets) socket.destroy();
      await new Promise(resolve => server.close(resolve));
    }
    await rm(root, { recursive: true, force: true });
  }
}

let passed = 0;
async function check(name, run) {
  await run();
  passed++;
  console.log('PASS ' + name);
}
for (const tool of tools) {
  for (const behavior of ['valid', 'transient', 'truncated']) {
    await check(`${tool}: ${behavior}`, async () => {
      const result = await scenario(tool, behavior);
      assert.equal(result.code, 0, result.stderr);
      assert.deepEqual(result.installed, payload);
      assert.equal(result.requests.length, behavior === 'valid' ? 1 : 2);
      assert(result.requests.every(request => request.prior === null), 'partial executable became visible during retry');
      assert.deepEqual(result.leftovers, []);
      if (tool === 'cargo-llvm-cov') assert.equal(result.rustup, 'component add llvm-tools-preview\n');
    });
  }
  await check(`${tool}: cached executable avoids network`, async () => {
    const existing = Buffer.from('#!/bin/sh\nexit 0\n');
    const result = await scenario(tool, 'permanent', { existing });
    assert.equal(result.code, 0, result.stderr);
    assert.deepEqual(result.installed, existing);
    assert.equal(result.requests.length, 0);
    assert.deepEqual(result.leftovers, []);
    if (tool === 'cargo-llvm-cov') assert.equal(result.rustup, 'component add llvm-tools-preview\n');
  });
}
for (const behavior of ['permanent', 'truncated-always', 'corrupt', 'digest-mismatch', 'metadata-json']) {
  for (const existing of [undefined, Buffer.from('#!/bin/sh\nprintf "existing-tool\\n"\n')]) {
    await check(`failed ${behavior}: preserves ${existing ? 'existing tool' : 'absence'}`, async () => {
      const result = await scenario('cargo-llvm-cov', behavior, { existing, direct: existing !== undefined });
      assert.notEqual(result.code, 0);
      assert.deepEqual(result.installed, existing ?? null);
      assert.equal(result.requests.length, ['corrupt', 'digest-mismatch', 'metadata-json'].includes(behavior) ? 1 : 4);
      assert(result.requests.every(request => existing ? request.prior?.equals(existing) : request.prior === null));
      assert.deepEqual(result.leftovers, []);
      assert(result.elapsed < 15000, 'failure retries must stay bounded');
    });
  }
}
console.log(`${passed} bootstrap checks passed`);
