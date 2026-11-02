'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { snapshot, fileDigest, publish, readBuild, listBuilds } = require('../lib/artifacts');

function command(name, args) {
  const result = spawnSync(name, args, { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr); return result.stdout.trim();
}
function repository(root, name, files) {
  const dir = path.join(root, name); fs.mkdirSync(dir);
  for (const [file, content] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, file)), { recursive: true });
    fs.writeFileSync(path.join(dir, file), content);
  }
  command('git', ['-C', dir, 'init', '-b', 'main']);
  command('git', ['-C', dir, 'add', '.']);
  command('git', ['-C', dir, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.test',
    '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture']);
  return dir;
}

test('source fingerprints include transitive package revisions and dirty/untracked contents', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-source-'));
  repository(root, 'Shared', { 'Package.swift': '// package' });
  const sdk = repository(root, 'SDK', { 'Package.swift': '.package(path: "../Shared")', 'Source.swift': 'let n = 1' });
  const app = repository(root, 'App', { 'Package.swift': '.package(path: "../SDK")', '.gitignore': 'build/\n' });
  const before = snapshot(app, { toolchain: false });
  assert.equal(before.sources.length, 3); assert.equal(before.anyDirty, false);
  assert.equal(before.fingerprint, snapshot(app, { toolchain: false }).fingerprint);
  fs.mkdirSync(path.join(app, 'build')); fs.writeFileSync(path.join(app, 'build', 'ignored'), 'cache');
  assert.equal(before.fingerprint, snapshot(app, { toolchain: false }).fingerprint);
  fs.writeFileSync(path.join(sdk, 'Source.swift'), 'let n = 2');
  const dirty = snapshot(app, { toolchain: false });
  assert.equal(dirty.anyDirty, true); assert.notEqual(dirty.fingerprint, before.fingerprint);
  fs.writeFileSync(path.join(sdk, 'New.swift'), 'let extra = 1');
  const untracked = snapshot(app, { toolchain: false });
  fs.writeFileSync(path.join(sdk, 'New.swift'), 'let extra = 2');
  assert.notEqual(snapshot(app, { toolchain: false }).fingerprint, untracked.fingerprint);
});

test('initialized submodule contents participate in the source fingerprint', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-submodule-'));
  const sub = repository(root, 'Library', { 'Source.swift': 'let value = 1' });
  const app = repository(root, 'App', { 'Package.swift': '// package' });
  command('git', ['-C', app, '-c', 'protocol.file.allow=always', 'submodule', 'add', sub, 'Library']);
  command('git', ['-C', app, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.test',
    '-c', 'commit.gpgsign=false', 'commit', '-am', 'submodule']);
  const before = snapshot(app, { toolchain: false });
  assert.equal(before.sources.length, 2); assert.equal(before.anyDirty, false);
  fs.writeFileSync(path.join(app, 'Library', 'Source.swift'), 'let value = 2');
  const after = snapshot(app, { toolchain: false });
  assert.equal(after.anyDirty, true); assert.notEqual(after.fingerprint, before.fingerprint);
  const library = after.sources.find(source => source.path.endsWith('/App/Library'));
  assert.equal(library.dirty, true);
});

test('new Xcode projects work without Git or an initial commit', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-new-project-'));
  fs.writeFileSync(path.join(root, 'Source.swift'), 'let value = 1');
  const bare = snapshot(root, { toolchain: false });
  assert.equal(bare.sources[0].kind, 'directory'); assert.equal(bare.sources[0].commit, null);
  fs.writeFileSync(path.join(root, 'Source.swift'), 'let value = 2');
  assert.notEqual(snapshot(root, { toolchain: false }).fingerprint, bare.fingerprint);
  command('git', ['-C', root, 'init', '-b', 'main']);
  command('git', ['-C', root, 'add', '.']);
  const unborn = snapshot(root, { toolchain: false });
  assert.equal(unborn.sources[0].kind, 'git'); assert.equal(unborn.sources[0].commit, null);
  fs.writeFileSync(path.join(root, 'Source.swift'), 'let value = 3');
  assert.notEqual(snapshot(root, { toolchain: false }).fingerprint, unborn.fingerprint);
});

test('resolved package changes affect provenance without exposing repository URLs', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-pins-'));
  const app = repository(root, 'App', { 'Package.swift': '// package', '.gitignore': 'Project.xcworkspace/\n' });
  fs.mkdirSync(path.join(app, 'Project.xcworkspace'));
  const pins = path.join(app, 'Project.xcworkspace', 'Package.resolved');
  fs.writeFileSync(pins, '{"revision":1,"url":"https://private.invalid"}');
  const before = snapshot(app, { toolchain: false });
  fs.writeFileSync(pins, '{"revision":2,"url":"https://private.invalid"}');
  assert.notEqual(before.fingerprint, snapshot(app, { toolchain: false }).fingerprint);
  assert.ok(!JSON.stringify(before).includes('https://private.invalid'));
});

test('the Git build stamp uses the captured commit even if HEAD advances', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-stamp-'));
  const app = repository(root, 'App', { 'Package.swift': '// package' });
  const before = snapshot(app, { toolchain: false });
  const file = path.join(root, 'before.json'); fs.writeFileSync(file, JSON.stringify(before));
  command('git', ['-C', app, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.test',
    '-c', 'commit.gpgsign=false', 'commit', '--allow-empty', '-m', 'advanced']);
  const stamp = command(process.execPath, [path.join(__dirname, '..', 'bin', 'build-manifest'), 'stamp', file, app]);
  assert.equal(stamp, command('git', ['-C', app, 'rev-parse', '--short', before.sources[0].commit]));
  assert.notEqual(stamp, command('git', ['-C', app, 'rev-parse', '--short', 'HEAD']));
});

test('a build manifest retains the exact artifact and binary UUIDs without replacing prior identities',
  { skip: process.platform !== 'darwin' && 'requires Apple binary tools' }, () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-artifact-'));
    const app = path.join(root, 'Example.app'); fs.mkdirSync(app);
    fs.writeFileSync(path.join(app, 'Info.plist'), JSON.stringify({ CFBundleIdentifier: 'com.example.installed',
      CFBundleName: 'Example', CFBundleVersion: 'abc1234', CFBundleShortVersionString: '1.0' }));
    const source = path.join(root, 'main.c'); fs.writeFileSync(source, 'int main(void) { return 0; }');
    command('xcrun', ['clang', source, '-o', path.join(app, 'Example')]);
    const artifact = path.join(root, 'example.zip');
    command('ditto', ['-c', '-k', '--keepParent', app, artifact]);
    const original = fileDigest(artifact);
    const options = { app, artifact, project: root, buildId: 'build-one', platform: 'macOS', configuration: 'Release',
      originalBundleId: 'com.example.original', before: { fingerprint: 'same' }, after: { fingerprint: 'same' }, store: path.join(root, 'store') };
    const result = publish(options);
    assert.equal(result.manifest.format, 'apparition-build/1');
    assert.equal(result.manifest.app.bundleId, 'com.example.installed');
    assert.equal(result.manifest.app.originalBundleId, 'com.example.original');
    assert.equal(result.manifest.source.stable, true);
    assert.equal(Object.keys(result.manifest.binaries).length, 1);
    assert.equal(readBuild('build-one', { store: options.store, verify: true }).manifest.buildId, 'build-one');
    assert.throws(() => readBuild('../escape', { store: options.store }), /invalid build identity/);
    const kept = path.join(path.dirname(result.path), result.manifest.artifact.file);
    assert.deepEqual(fileDigest(kept), original);
    const originalBytes = fs.readFileSync(kept);
    const altered = Buffer.from(originalBytes); altered[0] ^= 1;
    fs.writeFileSync(kept, altered);
    assert.throws(() => readBuild('build-one', { store: options.store, verify: true }), /integrity/);
    fs.writeFileSync(kept, originalBytes);
    fs.writeFileSync(artifact, 'next build');
    assert.deepEqual(fileDigest(kept), original);
    assert.throws(() => publish(options), /existing build manifest/);
    const changed = publish({ ...options, buildId: 'build-two', after: { fingerprint: 'changed' } });
    assert.equal(changed.manifest.source.stable, false);
    assert.deepEqual(fs.readdirSync(options.store).sort(), ['build-one', 'build-two']);
    const bad = path.join(options.store, 'invalid-build'); fs.mkdirSync(bad);
    fs.writeFileSync(path.join(bad, 'manifest.json'), '{}');
    fs.utimesSync(bad, new Date(), new Date(Date.now() + 1000));
    const listed = listBuilds({ store: options.store, limit: 1 });
    assert.equal(listed.length, 1); assert.notEqual(listed[0].buildId, 'invalid-build');
  });
