'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { createPresence, resolveInstallMode, validInstallMode, PRESENCE_TTL_MS } = require('../lib/install-mode');
const { ledgerEntryFromReport, ReportError } = require('../lib/installs');

test('an old request keeps its meaning: deploy false builds only, anything else installs from the Mac', () => {
  assert.equal(resolveInstallMode({ device: 'AVP' }), 'mac');
  assert.equal(resolveInstallMode({ deploy: true, device: 'AVP' }), 'mac');
  assert.equal(resolveInstallMode({ deploy: false, device: 'AVP' }), 'none');
});

test('an explicit mode is taken as given, whatever is open', () => {
  const presence = createPresence();
  presence.touch('AVP');
  for (const install of ['mac', 'device', 'none']) assert.equal(resolveInstallMode({ install, device: 'AVP', presence }), install);
  assert.equal(resolveInstallMode({ install: 'mac', deploy: false, device: 'AVP', presence }), 'mac');
});

test('auto goes to the device while its Apparition is open, and to the Xcode tunnel when it is closed', () => {
  const presence = createPresence();
  presence.touch('Vision Pro', 1000);
  assert.equal(resolveInstallMode({ install: 'auto', device: 'vision pro', presence, now: 1000 + PRESENCE_TTL_MS }), 'device');
  assert.equal(resolveInstallMode({ install: 'auto', device: 'Vision Pro', presence, now: 1001 + PRESENCE_TTL_MS }), 'mac');
  assert.equal(resolveInstallMode({ install: 'auto', device: 'iPhone', presence, now: 1000 }), 'mac');
  assert.equal(resolveInstallMode({ install: 'auto', device: 'iPhone' }), 'mac');
});

test('presence ignores anything that is not a plain device name and stays bounded', () => {
  const presence = createPresence({ max: 3 });
  assert.equal(presence.touch(''), false);
  assert.equal(presence.touch('   '), false);
  assert.equal(presence.touch('a\nb'), false);
  assert.equal(presence.touch('x'.repeat(129)), false);
  assert.equal(presence.touch(7), false);
  for (const name of ['a', 'b', 'c', 'd', 'e']) presence.touch(name, 5);
  assert.ok(presence.open(5).length <= 3);
  assert.deepEqual(presence.open(5 + PRESENCE_TTL_MS + 1), []);
});

test('only the four modes are valid', () => {
  assert.ok(['auto', 'mac', 'device', 'none', undefined].every(validInstallMode));
  assert.equal(validInstallMode('ota'), false);
  assert.throws(() => resolveInstallMode({ install: 'ota', device: 'AVP' }), /install must be one of/);
});

const repos = [{ name: 'Hypnos', platforms: [{ platform: 'visionOS', device: 'AVP', bundleId: 'com.example.hypnos' }] }];
const builds = [{ id: 7, repo: 'Hypnos', buildId: 'b-1', commit: 'abc1234', builtBranch: 'main' }];
const readBuild = id => {
  if (id !== 'b-1') throw new Error('no such build');
  return { manifest: { platform: 'visionOS', app: { bundleId: 'com.example.hypnos', version: '2.0', build: 'abc1234' } } };
};
const options = { repos, builds, readBuild, now: 99 };

test('a device-reported install becomes a confirmed ledger entry read from the service\'s own records', () => {
  const result = ledgerEntryFromReport({ device: 'AVP', bundleId: 'com.example.hypnos', buildId: 'b-1' }, options);
  assert.deepEqual(result, { device: 'AVP', bundleId: 'com.example.hypnos', entry: {
    repo: 'Hypnos', branch: 'main', commit: 'abc1234', buildId: 'b-1', platform: 'visionOS',
    version: '2.0', buildNo: 'abc1234', at: 99, confirmed: true, via: 'apparition' } });
});

test('a report that names anything unconfigured, unkept or foreign is refused', () => {
  const bad = (report, pattern, status) => assert.throws(() => ledgerEntryFromReport(report, options),
    error => error instanceof ReportError && pattern.test(error.message) && error.status === status);
  const ok = { device: 'AVP', bundleId: 'com.example.hypnos', buildId: 'b-1' };
  bad({ ...ok, extra: 1 }, /Name a configured/, 400);
  bad({ ...ok, buildId: '../x' }, /Name a configured/, 400);
  bad({ ...ok, device: 'iPhone' }, /not a configured target/, 400);
  bad({ ...ok, bundleId: 'com.example.other' }, /not a configured target/, 400);
  bad({ ...ok, platform: 'iOS' }, /not a configured target/, 400);
  bad({ ...ok, buildId: 'b-2' }, /not kept/, 404);
  assert.throws(() => ledgerEntryFromReport(ok, { ...options, readBuild: () => ({ manifest: { platform: 'visionOS', app: { bundleId: 'com.example.else' } } }) }),
    /not of this target/);
  assert.throws(() => ledgerEntryFromReport(ok, { ...options, readBuild: () => ({ manifest: { platform: 'iOS', app: { bundleId: 'com.example.hypnos' } } }) }),
    /another platform/);
});
