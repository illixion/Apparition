'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { TMP } = require('./helpers'); // sets the env first
const { staleClones, siblingPackages } = require('../server');

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

test('a build clone excludes a live source index lock without removing it', async () => {
  const src = fs.mkdtempSync(path.join(os.tmpdir(), 'clone-lock-src-'));
  const dst = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'clone-lock-dst-')), 'repo');
  fs.mkdirSync(path.join(src, '.git', 'objects'), { recursive: true });
  fs.writeFileSync(path.join(src, '.git', 'index.lock'), 'active source writer');
  fs.writeFileSync(path.join(src, '.git', 'index'), 'index');
  fs.writeFileSync(path.join(src, '.git', 'objects', 'object'), 'object');
  await cloneTree(src, dst, [], async (from, to) => fs.cpSync(from, to, { recursive: true }));
  assert.equal(fs.readFileSync(path.join(src, '.git', 'index.lock'), 'utf8'), 'active source writer');
  assert.ok(!fs.existsSync(path.join(dst, '.git', 'index.lock')));
  assert.equal(fs.readFileSync(path.join(dst, '.git', 'index'), 'utf8'), 'index');
  assert.equal(fs.readFileSync(path.join(dst, '.git', 'objects', 'object'), 'utf8'), 'object');
});

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

test('a clone links the packages outside the repo it builds against, transitively', () => {
  const projects = fs.mkdtempSync(path.join(TMP, 'projects-'));
  const write = (rel, text) => { fs.mkdirSync(path.dirname(path.join(projects, rel)), { recursive: true }); fs.writeFileSync(path.join(projects, rel), text); };
  // the app's project references a sibling package and one of its own
  write('App/App/App.xcodeproj/project.pbxproj', `isa = XCLocalSwiftPackageReference;\n\t\t\trelativePath = ../../SDK;\n
    isa = XCLocalSwiftPackageReference;\n\t\t\trelativePath = "../Packages/Local";`);
  write('App/Packages/Local/Package.swift', '.package(path: "../../../Trace")');
  // SDK pulls in Trace; Missing has no checkout; build/ is never scanned
  write('SDK/Package.swift', '.package(name: "Trace", path: "../Trace"), .package(path: "../Missing")');
  write('Trace/Package.swift', '');
  write('App/build/Stale/Package.swift', '.package(path: "../../../Other")');
  fs.mkdirSync(path.join(projects, 'Other'));
  // the clone sits in a slot elsewhere, so its own references resolve there
  const slot = fs.mkdtempSync(path.join(TMP, 'slot-'));
  fs.cpSync(path.join(projects, 'App'), path.join(slot, 'App'), { recursive: true });
  assert.deepEqual(siblingPackages(path.join(slot, 'App'), projects), ['SDK', 'Trace']);
});
