'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

test('immutable symbol sets retain three real builds, reject identity reuse and publish atomically',
  { skip: process.platform !== 'darwin' && 'requires Apple DWARF tools' }, () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-symbols-'));
    const products = path.join(root, 'products');
    const bundle = path.join(products, 'Example.app');
    const archive = path.join(root, 'archive');
    fs.mkdirSync(bundle, { recursive: true });
    const script = fs.readFileSync(path.join(__dirname, '..', 'lib', 'symbols.sh'), 'utf8');
    const fn = script.slice(script.indexOf('archive_dsyms() {'));
    assert.match(fn, /immutable dSYMs/);
    const command = (name, args) => {
      const result = spawnSync(name, args, { encoding: 'utf8' });
      assert.equal(result.status, 0, result.stderr); return result.stdout;
    };
    const unrelated = path.join(root, 'unrelated.c');
    fs.writeFileSync(unrelated, 'int main(void) { return 99; }\n');
    command('xcrun', ['clang', '-g', unrelated, '-o', path.join(root, 'Unrelated')]);
    command('xcrun', ['dsymutil', path.join(root, 'Unrelated'), '-o', path.join(products, 'Unrelated.dSYM')]);
    const records = [];
    const archiveBuild = key => spawnSync('/opt/homebrew/bin/bash', ['-c', fn + '\narchive_dsyms'], {
      encoding: 'utf8', env: { PATH: '/usr/bin:/bin', HOME: root, APP_PATH: bundle, APP_BUNDLE: bundle,
        DEBUGTRACE_DSYM_DIR: archive, TRACE_KEY_ID: key, BUILD_BUNDLE_ID: 'com.example.app',
        DEVICE_NAME: 'Headset', PLATFORM: 'visionOS', CONFIG: 'Release' },
    });
    for (let i = 1; i <= 3; ++i) {
      const source = path.join(root, `v${i}.c`);
      fs.writeFileSync(source, `int main(void) { return ${i}; }\n`);
      const binary = path.join(bundle, 'Example');
      command('xcrun', ['clang', '-g', source, '-o', binary]);
      command('xcrun', ['dsymutil', binary, '-o', path.join(products, 'Example.dSYM')]);
      fs.writeFileSync(path.join(bundle, 'Info.plist'), JSON.stringify({ CFBundleVersion: `v${i}` }));
      const result = archiveBuild(`build-${i}`);
      assert.equal(result.status, 0, result.stderr);
      const slot = path.join(archive, `com.example.app@build-${i}`);
      const record = JSON.parse(fs.readFileSync(path.join(slot, 'uuids.json')));
      assert.equal(record.buildId, `build-${i}`);
      assert.equal(record.build, `v${i}`);
      assert.equal(record.platform, 'visionOS');
      assert.equal(Object.keys(record.uuids).length, 1);
      assert.ok(!fs.existsSync(path.join(slot, 'Unrelated.dSYM')), 'stale sibling-target symbols stay out');
      for (const info of Object.values(record.uuids)) {
        assert.ok(fs.existsSync(path.join(slot, info.dwarf)));
        assert.match(info.sha256, /^[a-f0-9]{64}$/);
        assert.equal(info.bytes, fs.statSync(path.join(slot, info.dwarf)).size);
      }
      records.push(record);
    }
    assert.equal(new Set(records.flatMap(record => Object.keys(record.uuids))).size, 3);
    for (let i = 1; i <= 3; ++i) assert.ok(fs.existsSync(path.join(archive, `com.example.app@build-${i}`, 'uuids.json')));
    assert.equal(archiveBuild('build-3').status, 0, 'same symbol set is idempotent');
    assert.notEqual(archiveBuild('build-1').status, 0, 'a build identity cannot be overwritten');
    assert.deepEqual(fs.readdirSync(archive).sort(), ['com.example.app@build-1', 'com.example.app@build-2', 'com.example.app@build-3']);
    assert.deepEqual(JSON.parse(fs.readFileSync(path.join(archive, 'com.example.app@build-1', 'uuids.json'))), records[0]);
  });
