'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
require('./helpers'); // sets the env first
const { staleClones } = require('../server');

const DAY = 86400000;
const now = 100 * DAY;

test('only clones untouched for longer than the limit are stale', () => {
  const entries = [
    { dir: '/c/App/old', mtimeMs: now - 8 * DAY },
    { dir: '/c/App/fresh', mtimeMs: now - 2 * DAY },
  ];
  assert.deepEqual(staleClones(entries, now, 7 * DAY, new Set()), ['/c/App/old']);
});

test('a clone a build is using is never stale, however old', () => {
  const entries = [{ dir: '/c/App/busy', mtimeMs: now - 30 * DAY }];
  assert.deepEqual(staleClones(entries, now, 7 * DAY, new Set(['/c/App/busy'])), []);
});

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { normalizeExcludes, cloneTree } = require('../server');

test('cloneExclude paths are cleaned and anything outside the repo is dropped', () => {
  assert.deepEqual(normalizeExcludes(['build', './target/', 'clients//android', '../x', '.git', '', 3, '/abs']),
    ['build', 'target', 'clients/android', 'abs']);
  assert.deepEqual(normalizeExcludes(undefined), []);
});

test('a clone leaves out the excluded paths and copies everything else whole', async () => {
  const src = fs.mkdtempSync(path.join(os.tmpdir(), 'clone-src-'));
  const dst = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'clone-dst-')), 'out');
  const put = (rel) => { fs.mkdirSync(path.dirname(path.join(src, rel)), { recursive: true }); fs.writeFileSync(path.join(src, rel), rel); };
  for (const f of ['a.txt', 'build/x', 'target/debug/y', 'clients/android/z', 'clients/ios/keep', 'clients/ios/build/w', 'src/deep/k']) put(f);
  const copied = [];
  const copy = async (from, to) => { copied.push(path.relative(src, from)); fs.cpSync(from, to, { recursive: true }); };
  try {
    await cloneTree(src, dst, ['build', 'target', 'clients/android', 'clients/ios/build'], copy);
    const files = [];
    (function walk(d) { for (const e of fs.readdirSync(d, { withFileTypes: true })) e.isDirectory() ? walk(path.join(d, e.name)) : files.push(path.relative(dst, path.join(d, e.name))); })(dst);
    assert.deepEqual(files.sort(), ['a.txt', 'clients/ios/keep', 'src/deep/k']);
    // Whole directories with nothing excluded beneath them are one copy, not one per file.
    assert.ok(copied.includes('src') && copied.includes(path.join('clients', 'ios', 'keep')));
    assert.ok(!copied.includes('clients'));
  } finally {
    fs.rmSync(src, { recursive: true, force: true });
    fs.rmSync(path.dirname(dst), { recursive: true, force: true });
  }
});
