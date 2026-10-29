'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { capture, register, configure, platformOf } = require('../lib/xcode');
const { fileDigest, readBuild } = require('../lib/artifacts');

function command(name, args) {
  const result = spawnSync(name, args, { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr); return result.stdout.trim();
}

test('the optional XcodeGen spec preserves the original and scopes both actions to the target', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-xcode-spec-'));
  const spec = path.join(root, 'project.yml');
  const original = 'name: Fixture\ntargets: {}\n'; fs.writeFileSync(spec, original);
  const generated = configure({ spec, target: 'Fixture', generate: false });
  assert.equal(fs.readFileSync(spec, 'utf8'), original);
  const overlay = JSON.parse(fs.readFileSync(generated.spec));
  assert.deepEqual(overlay.include, [{ path: spec, relativePaths: false }]);
  const build = overlay.schemes['Fixture (Apparition)'].build;
  assert.equal(build.preActions[0].settingsTarget, 'Fixture');
  assert.equal(build.postActions[0].settingsTarget, 'Fixture');
  assert.match(build.preActions[0].script, /xcode snapshot/);
  assert.match(build.postActions[0].script, /xcode register/);
  assert.doesNotMatch(build.postActions[0].script, /xcodebuild|--run|--jit|codesign/);
  assert.deepEqual(configure({ spec, target: 'Fixture', generate: false }), generated);
  fs.writeFileSync(generated.spec, '# user modification');
  assert.throws(() => configure({ spec, target: 'Fixture', generate: false }), /different content/);
  assert.equal(fs.readFileSync(generated.spec, 'utf8'), '# user modification');
});

test('XcodeGen resolves a nested optional spec against the original project root',
  { skip: process.platform !== 'darwin' && 'requires XcodeGen' }, () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-xcodegen-'));
    const spec = path.join(root, 'project.yml');
    fs.writeFileSync(spec, 'name: Fixture\ntargets:\n  Fixture:\n    type: application\n    platform: macOS\n');
    configure({ spec, target: 'Fixture', output: path.join(root, 'build', 'optional.yml') });
    const scheme = fs.readFileSync(path.join(root, 'Fixture.xcodeproj', 'xcshareddata', 'xcschemes', 'Fixture (Apparition).xcscheme'), 'utf8');
    assert.match(scheme, /Capture Apparition source context/);
    assert.match(scheme, /Register with Apparition/);
    assert.match(scheme, /EnvironmentBuildable/);
  });

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-xcode-product-'));
  const project = path.join(root, 'Project'); fs.mkdirSync(project);
  const source = path.join(project, 'main.c'); fs.writeFileSync(source, 'int main(void) { return 0; }');
  const app = path.join(root, 'Products', 'Fixture.app');
  fs.mkdirSync(path.join(app, 'Contents', 'MacOS'), { recursive: true });
  fs.writeFileSync(path.join(app, 'Contents', 'Info.plist'), JSON.stringify({
    CFBundleIdentifier: 'com.example.xcode', CFBundleExecutable: 'Fixture', CFBundleName: 'Fixture',
    CFBundlePackageType: 'APPL', CFBundleVersion: '1', CFBundleShortVersionString: '1.0', LSMinimumSystemVersion: '14.0',
  }));
  const executable = path.join(app, 'Contents', 'MacOS', 'Fixture');
  command('xcrun', ['clang', '-g', '-mmacosx-version-min=14.0', source, '-o', executable]);
  command('codesign', ['--remove-signature', executable]);
  const symbols = app + '.dSYM'; command('dsymutil', [executable, '-o', symbols]);
  return { root, project, app, executable, symbols, store: path.join(root, 'store'), symbolStore: path.join(root, 'symbols') };
}

test('unsigned device-ready products register without being re-signed or claiming compile provenance',
  { skip: process.platform !== 'darwin' && 'requires Xcode tools' }, () => {
    const f = fixture(), before = fileDigest(f.executable);
    const result = register(f);
    assert.equal(result.manifest.platform, 'macOS');
    assert.equal(result.manifest.origin.signature.state, 'unsigned');
    assert.equal(result.manifest.source, null);
    assert.ok(result.manifest.origin.observedSource.fingerprint);
    assert.deepEqual(fileDigest(f.executable), before);
    assert.ok(result.manifest.symbols); assert.equal(Object.keys(result.manifest.symbols.uuids).length, 1);
    assert.equal(readBuild(result.manifest.buildId, { store: f.store, verify: true }).manifest.app.build, '1');
  });

test('simulator products cannot be registered as device artifacts',
  { skip: process.platform !== 'darwin' && 'requires Xcode tools' }, () => {
    const f = fixture();
    const sdk = command('xcrun', ['--sdk', 'iphonesimulator', '--show-sdk-path']);
    command('xcrun', ['clang', '-target', 'arm64-apple-ios17.0-simulator', '-isysroot', sdk,
      path.join(f.project, 'main.c'), '-o', f.executable]);
    assert.throws(() => platformOf(f.app, { CFBundleExecutable: 'Fixture' }), /not a simulator/);
  });

test('signed products and their retained zip keep the original signature and context identity',
  { skip: process.platform !== 'darwin' && 'requires Xcode tools' }, () => {
    const f = fixture();
    const entitlements = path.join(f.root, 'entitlements.plist');
    fs.writeFileSync(entitlements, '<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.get-task-allow</key><true/></dict></plist>');
    command('codesign', ['--force', '--sign', '-', '--entitlements', entitlements, f.app]);
    const binary = fileDigest(f.executable), context = path.join(f.root, 'context.json');
    const recorded = capture(f.project, context);
    const result = register({ ...f, context });
    assert.equal(result.manifest.buildId, recorded.buildId);
    assert.equal(result.manifest.origin.signature.state, 'signed');
    assert.equal(result.manifest.origin.signature.debuggable, true);
    assert.equal(result.manifest.source.stable, true);
    assert.deepEqual(fileDigest(f.executable), binary);
    command('codesign', ['--verify', '--deep', '--strict', f.app]);
    const archive = readBuild(recorded.buildId, { store: f.store, verify: true });
    const extracted = path.join(f.root, 'extracted'); command('ditto', ['-x', '-k', archive.file, extracted]);
    command('codesign', ['--verify', '--deep', '--strict', path.join(extracted, 'Fixture.app')]);
    assert.throws(() => register({ ...f, context }), /existing build manifest/);
    const other = path.join(f.root, 'Other'); fs.mkdirSync(other);
    assert.throws(() => register({ ...f, project: other, context }), /mismatched Xcode source context/);
    fs.writeFileSync(path.join(f.app, 'Contents', 'modified.txt'), 'not sealed');
    assert.throws(() => register(f), /codesign failed/);
  });
