'use strict';
// `quietAppsDevices`: devices whose app list comes from `apparition apps` (Apparition's own
// pairing and tunnel, which CoreDevice never sees) instead of `devicectl device info apps`.
// Driven here with a fake apparition command line.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
require('./helpers');

// The server reads its config once at require time: extend the harness config before loading it.
const cfgPath = process.env.APPSTORE_CONFIG;
const tmp = path.dirname(cfgPath);
const fakeCli = path.join(tmp, 'fake-apparition');
fs.writeFileSync(fakeCli, `#!/bin/sh
# fake apparition: "apps --device ATV --json" → two apps; any other device → the CLI's error line
[ "$1" = "apps" ] && [ "$2" = "--device" ] && [ "$4" = "--json" ] || { echo "error: unexpected arguments $*" >&2; exit 2; }
[ "$3" = "ATV" ] || { echo "error: no paired device called $3" >&2; exit 1; }
echo '{"device":"313f","name":"ATV","apps":[
 {"bundle_id":"com.example.tv","name":"TV App","version":"1.0","build":"abc1234"},
 {"bundle_id":"org.other.thing","name":"Other","version":"2","build":"9"}]}'
`);
fs.chmodSync(fakeCli, 0o755);
const cfg = JSON.parse(fs.readFileSync(cfgPath, 'utf8'));
cfg.apparitionCli = fakeCli;
cfg.quietAppsDevices = ['ATV', 'Den TV'];
cfg.ledgerOnlyDevices = ['ATV', 'Bedroom TV'];
fs.writeFileSync(cfgPath, JSON.stringify(cfg));

const { quietAppsDevices, skippedDevices, refreshDeviceApps, _state } = require('../server');

test('quietAppsDevices keeps only device-name strings', () => {
  assert.deepEqual(quietAppsDevices({ quietAppsDevices: ['ATV', '', 3, null, 'Den TV'] }), ['ATV', 'Den TV']);
  assert.deepEqual(quietAppsDevices({}), []);
  assert.deepEqual(quietAppsDevices({ quietAppsDevices: { ATV: 'x' } }), []);
  assert.deepEqual(quietAppsDevices(undefined), []);
});

test('a ledger-only device with a quiet route is no longer skipped', () => {
  assert.deepEqual(skippedDevices(cfg), ['Bedroom TV']);
  assert.deepEqual(skippedDevices({ ledgerOnlyDevices: ['ATV'] }), ['ATV']);
});

test('a quiet device is read through apparition, mapped to the inventory shape and prefix-filtered, even without force', async () => {
  await refreshDeviceApps('ATV');
  const inv = _state().deviceApps.ATV;
  assert.ok(inv && inv.fetchedAt > 0 && !inv.error, JSON.stringify(inv));
  assert.deepEqual(inv.apps, { 'com.example.tv': { name: 'TV App', version: '1.0', buildNo: 'abc1234' } });
});

test('the CLI\'s error line lands on the device entry instead of a devicectl call', async () => {
  await refreshDeviceApps('Den TV');
  const inv = _state().deviceApps['Den TV'];
  assert.equal(inv.error, 'no paired device called Den TV');
  assert.ok(inv.erroredAt > 0);
});

test('a ledger-only device without a quiet route is still skipped', async () => {
  await refreshDeviceApps('Bedroom TV');
  assert.equal(_state().deviceApps['Bedroom TV'], undefined);
});

test('build-and-sign is told which devices to install through apparition push', () => {
  const { quietInstallEnv } = require('../server');
  assert.deepEqual(quietInstallEnv(cfg), { QUIET_DEVICES: 'ATV,Den TV', APPARITION_CLI: fakeCli });
  assert.deepEqual(quietInstallEnv({}), {});
});
