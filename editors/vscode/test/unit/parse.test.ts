import { strict as assert } from 'assert';
import { test } from 'node:test';
import { OakError, parseJsonDocument, stripAnsi } from '../../src/oak';
import {
  commitSubject,
  normalizeLog,
  normalizeStatusChanges,
  pushStateFrom,
  relativeTime,
  statusLetter,
  validateBranchName,
  validateRepoSpec,
} from '../../src/parse';

test('statusLetter maps oak statuses', () => {
  assert.equal(statusLetter('added'), 'A');
  assert.equal(statusLetter('modified'), 'M');
  assert.equal(statusLetter('deleted'), 'D');
  assert.equal(statusLetter('renamed'), 'R');
  assert.equal(statusLetter('conflicted'), '!');
  assert.equal(statusLetter('weird'), 'W');
});

test('normalizeStatusChanges sorts and drops malformed rows', () => {
  const out = normalizeStatusChanges([
    { path: 'b.txt', status: 'modified' },
    { path: 'a.txt', status: 'added' },
    { nope: true },
    null,
  ]);
  assert.deepEqual(
    out.map((c) => c.path),
    ['a.txt', 'b.txt'],
  );
  assert.deepEqual(normalizeStatusChanges(undefined), []);
});

test('normalizeLog accepts arrays and {commits} wrappers', () => {
  const entry = { hash: 'abc', timestamp: '2026-01-01T00:00:00Z' };
  assert.equal(normalizeLog([entry]).length, 1);
  assert.equal(normalizeLog({ commits: [entry, { bad: 1 }] }).length, 1);
  assert.equal(normalizeLog('x').length, 0);
});

test('commitSubject uses the first description line, else the hash', () => {
  assert.equal(commitSubject({ hash: 'a'.repeat(64), timestamp: '', description_or_subject: 'Title\n\nBody' }), 'Title');
  assert.equal(commitSubject({ hash: 'b'.repeat(64), timestamp: '' }), 'b'.repeat(12));
});

test('relativeTime', () => {
  const now = Date.parse('2026-10-06T12:00:00Z');
  assert.equal(relativeTime('2026-10-06T11:59:30Z', now), 'just now');
  assert.equal(relativeTime('2026-10-06T11:58:00Z', now), '2 minutes ago');
  assert.equal(relativeTime('2026-10-05T12:00:00Z', now), '1 day ago');
  assert.equal(relativeTime('not a date', now), 'not a date');
});

test('validateBranchName', () => {
  assert.equal(validateBranchName('my-feature'), undefined);
  assert.equal(validateBranchName('team/feature'), undefined);
  assert.ok(validateBranchName(''));
  assert.ok(validateBranchName('has space'));
  assert.ok(validateBranchName('a..b'));
  assert.ok(validateBranchName('-lead'));
  assert.ok(validateBranchName('x.lock'));
});

test('validateRepoSpec', () => {
  assert.equal(validateRepoSpec('oak/oak'), undefined);
  assert.ok(validateRepoSpec('oak'));
  assert.ok(validateRepoSpec('a/b/c'));
});

test('parseJsonDocument tolerates leading human output', () => {
  assert.deepEqual(parseJsonDocument('{"a":1}'), { a: 1 });
  assert.deepEqual(parseJsonDocument('Pushing...\nDone\n{"a":2}\n'), { a: 2 });
  assert.deepEqual(parseJsonDocument('note\n[\n  {"a":3}\n]\n'), [{ a: 3 }]);
  assert.throws(() => parseJsonDocument('nothing here'));
});

test('OakError.describe prefers JSON errors, then error lines', () => {
  assert.equal(OakError.describe(1, '{"error":{"message":"boom"}}', ''), 'boom');
  assert.equal(OakError.describe(6, '', 'progress\nerror: not logged in\nhint: run oak login'), 'not logged in\nhint: run oak login');
  assert.equal(OakError.describe(2, '', ''), 'oak exited with code 2');
  const err = new OakError(['push'], 6, '', 'error: offline');
  assert.ok(err.isNetwork);
  assert.ok(!err.isConflict);
});

test('stripAnsi', () => {
  assert.equal(stripAnsi('\x1b[1;32mok\x1b[0m'), 'ok');
});

test('pushStateFrom reads needs_push / unpushed_commit_count (absent = default)', () => {
  assert.equal(pushStateFrom(undefined, 'b'), undefined);
  assert.equal(pushStateFrom({ branch: 'other' }, 'b'), undefined, 'stale document for another branch');
  assert.deepEqual(pushStateFrom({ branch: 'b', current_branch_pushed_head: 'h1' }, 'b'), {
    needsPush: false,
    unpushed: 0,
    published: true,
  });
  assert.deepEqual(pushStateFrom({ branch: 'b', needs_push: true, unpushed_commit_count: 2, current_branch_pushed_head: 'h0' }, 'b'), {
    needsPush: true,
    unpushed: 2,
    published: true,
  });
  assert.deepEqual(pushStateFrom({ branch: 'b', needs_push: true, unpushed_commit_count: 1 }, 'b'), {
    needsPush: true,
    unpushed: 1,
    published: false,
  });
});
