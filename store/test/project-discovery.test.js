'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { discover, confText } = require('../lib/project-discovery');

function repo() {
  const directory = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-discovery-')));
  execFileSync('git', ['init', '-b', 'main', directory], { stdio: 'ignore' });
  return directory;
}

test('discovery reports a checkout with no Xcode project without running xcodebuild', () => {
  const directory = repo();
  fs.writeFileSync(path.join(directory, 'Package.swift'), '// swift-tools-version:5.9\n');
  const found = discover(directory, []);
  assert.equal(found.container, null);
  assert.match(found.note, /Swift package/);
  assert.equal(found.hasConf, false);
  assert.equal(found.registered, false);
  assert.deepEqual(discover(directory, [{ path: directory }]).registered, true);
});

test('discovery refuses a subdirectory, a relative path and a missing directory', () => {
  const directory = repo();
  fs.mkdirSync(path.join(directory, 'Sub'));
  assert.throws(() => discover(path.join(directory, 'Sub'), []), /root/);
  assert.throws(() => discover('relative', []), /absolute/);
  assert.throws(() => discover(path.join(directory, 'Nope'), []), /does not exist/);
});

test('a generated conf quotes its values and holds no signing material', () => {
  const text = confText({ container: 'A B.xcodeproj', scheme: "it's $(x)", platform: 'visionOS', device: 'Vision "Pro" $x' });
  assert.match(text, /^PROJECT_PATH='A B.xcodeproj'$/m);
  assert.match(text, /^SCHEME_NAME='it'\\''s \$\(x\)'$/m);
  assert.match(text, /DEVICE_NAME="\$\{DEVICE_NAME:-Vision \\"Pro\\" \\\$x\}"/);
  assert.ok(!/^(TEAM_ID|DEV_P12_PATH|DEV_PROFILE_PATH)=/m.test(text));
});
