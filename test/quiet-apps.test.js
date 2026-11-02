'use strict';
// `quietAppsDevices`: devices whose app list comes from bin/quiet-apps (a tunnel CoreDevice never
// sees) instead of `devicectl device info apps`. Driven here with a fake tool.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
require('./helpers');

// The server reads its config once at require time: extend the harness config before loading it.
const cfgPath = process.env.APPSTORE_CONFIG;
const tmp = path.dirname(cfgPath);
const fakeTool = path.join(tmp, 'fake-quiet-apps');
fs.writeFileSync(fakeTool, `#!/bin/sh
# fake quiet-apps: --identifier good → two apps; anything else → the tool's error shape
[ "$2" = "good" ] || { echo '{"error": "no pymobiledevice3 pairing record '"$2"'"}'; exit 78; }
echo '{"result":{"apps":[
 {"bundleIdentifier":"com.example.tv","name":"TV App","version":"1.0","bundleVersion":"abc1234"},
 {"bundleIdentifier":"org.other.thing","name":"Other","version":"2","bundleVersion":"9"}]}}'
`);
fs.chmodSync(fakeTool, 0o755);
const cfg = JSON.parse(fs.readFileSync(cfgPath, 'utf8'));
cfg.quietAppsTool = fakeTool;
cfg.quietAppsDevices = { ATV: 'good', 'Den TV': 'stale' };
cfg.ledgerOnlyDevices = ['ATV', 'Bedroom TV'];
fs.writeFileSync(cfgPath, JSON.stringify(cfg));

const { quietAppsDevices, skippedDevices, refreshDeviceApps, _state } = require('../server');

test('quietAppsDevices keeps only name → non-empty string entries', () => {
  assert.deepEqual(quietAppsDevices({ quietAppsDevices: { ATV: 'abc', X: '', Y: 3, Z: null } }), { ATV: 'abc' });
  assert.deepEqual(quietAppsDevices({}), {});
  assert.deepEqual(quietAppsDevices({ quietAppsDevices: ['ATV'] }), {});
  assert.deepEqual(quietAppsDevices(undefined), {});
});

test('a ledger-only device with a quiet route is no longer skipped', () => {
  assert.deepEqual(skippedDevices(cfg), ['Bedroom TV']);
  assert.deepEqual(skippedDevices({ ledgerOnlyDevices: ['ATV'] }), ['ATV']);
});

test('a quiet device is read through the tool, filtered by bundle prefix, even without force', async () => {
  await refreshDeviceApps('ATV');
  const inv = _state().deviceApps.ATV;
  assert.ok(inv && inv.fetchedAt > 0 && !inv.error, JSON.stringify(inv));
  assert.deepEqual(inv.apps, { 'com.example.tv': { name: 'TV App', version: '1.0', buildNo: 'abc1234' } });
});

test('the tool\'s error lands on the device entry instead of a devicectl call', async () => {
  await refreshDeviceApps('Den TV');
  const inv = _state().deviceApps['Den TV'];
  assert.match(inv.error, /no pymobiledevice3 pairing record stale/);
  assert.ok(inv.erroredAt > 0);
});

test('a ledger-only device without a quiet route is still skipped', async () => {
  await refreshDeviceApps('Bedroom TV');
  assert.equal(_state().deviceApps['Bedroom TV'], undefined);
});
