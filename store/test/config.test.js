'use strict';
const { test, describe } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { TMP } = require('./helpers');
const { resolveBuildSettings, normalizePrefixes } = require('../server');

describe('resolveBuildSettings', () => {
  const rb = (bs, branch = 'main', platform = 'iOS') => resolveBuildSettings({ buildSettings: bs }, branch, platform);

  test('none configured', () => {
    assert.deepEqual(resolveBuildSettings({}, 'main', 'iOS'), []);
  });
  test('an array applies to every branch and platform', () => {
    assert.deepEqual(rb(['A=1']), ['A=1']);
    assert.deepEqual(rb(['A=1'], 'dev', 'visionOS'), ['A=1']);
  });
  test('per-branch, with "*" as the default', () => {
    const bs = { feature: ['F=1'], '*': ['D=1'] };
    assert.deepEqual(rb(bs, 'feature'), ['F=1']);
    assert.deepEqual(rb(bs, 'main'), ['D=1']);
  });
  test('per-branch without a default gives nothing for other branches', () => {
    assert.deepEqual(rb({ feature: ['F=1'] }, 'main'), []);
  });
  test('per-platform within a branch, with "*" as the default', () => {
    const bs = { main: { visionOS: ['V=1'], '*': ['D=1'] } };
    assert.deepEqual(rb(bs, 'main', 'visionOS'), ['V=1']);
    assert.deepEqual(rb(bs, 'main', 'iOS'), ['D=1']);
  });
  test('per-platform without a default gives nothing for other platforms', () => {
    assert.deepEqual(rb({ '*': { visionOS: ['V=1'] } }, 'main', 'iOS'), []);
  });
  test('a branch entry wins over "*" even when the platform is not listed in it', () => {
    const bs = { main: { visionOS: ['V=1'] }, '*': { iOS: ['I=1'] } };
    assert.deepEqual(rb(bs, 'main', 'iOS'), []);
  });
});

describe('normalizePrefixes', () => {
  test('a string becomes a one-element array', () => assert.deepEqual(normalizePrefixes('com.example'), ['com.example']));
  test('an array is kept', () => assert.deepEqual(normalizePrefixes(['a', 'b']), ['a', 'b']));
});

describe('starting the server', () => {
  test('a missing config.json exits with a clear message', () => {
    const r = spawnSync(process.execPath, [path.join(__dirname, '..', 'server.js')], {
      env: { ...process.env, STORE_CONFIG: path.join(TMP, 'does-not-exist.json'), STORE_DATA: path.join(TMP, 'data2') },
      encoding: 'utf8', timeout: 10000,
    });
    assert.equal(r.status, 1);
    assert.match(r.stderr, /config\.json not found/);
    assert.match(r.stderr, /config\.example\.json/);
  });
  test('requiring server.js does not start listening', () => {
    // helpers.js already required it with port 0; reaching here without a hang is the check,
    // plus the process must be able to exit on its own.
    const r = spawnSync(process.execPath, ['-e', "require('./server'); console.log('loaded')"], {
      cwd: path.join(__dirname, '..'),
      env: { ...process.env, STORE_CONFIG: path.join(TMP, 'config.json'), STORE_DATA: path.join(TMP, 'data3') },
      encoding: 'utf8', timeout: 10000,
    });
    assert.equal(r.status, 0, r.stderr);
    assert.match(r.stdout, /loaded/);
  });
});
