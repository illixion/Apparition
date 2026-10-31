'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const resign = require('../lib/resign');

const MISMATCH = 'Upgrade\'s application-identifier entitlement string (T1.com.example.Other) does not match installed application\'s application-identifier string (T1.com.example.app); rejecting upgrade.';
const profilePlist = (over = {}) => ({
  Name: 'my prof', TeamName: 'Team', TeamIdentifier: ['T1'], ExpirationDate: '2027-09-27T05:26:10Z',
  Platform: ['iOS', 'xrOS', 'visionOS'], ProvisionedDevices: ['UDID-1'],
  Entitlements: { 'application-identifier': 'T1.com.example.Other', 'get-task-allow': true }, ...over,
});

test('profiles are listed from the folder, without paths or device lists in the public view', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'resign-profiles-'));
  for (const name of ['a.mobileprovision', 'old.mobileprovision', 'notes.txt', '.hidden.mobileprovision', 'bad name.mobileprovision']) fs.writeFileSync(path.join(dir, name), 'x');
  const decode = file => path.basename(file) === 'old.mobileprovision' ? profilePlist({ ExpirationDate: '2020-01-01T00:00:00Z' }) : profilePlist();
  const profiles = resign.listProfiles({ dir, decode, now: Date.parse('2026-10-31') });
  assert.deepEqual(profiles.map(p => [p.id, p.expired]), [['a', false], ['old', true]]);
  assert.equal(profiles[0].appId, 'com.example.Other');
  assert.equal(profiles[0].debuggable, true);
  const shown = resign.publicProfile(profiles[0]);
  assert.equal(shown.file, undefined);
  assert.equal(shown.devices, undefined);
  assert.equal(shown.deviceCount, 1);
  assert.equal(resign.findProfile('../a', { dir, decode }), null);
  assert.equal(resign.findProfile('a', { dir, decode }).id, 'a');
});

test('a profile that expired, misses the device or is for another platform is refused early', () => {
  const profile = { ...resign.summarize('p', profilePlist()), expired: false };
  assert.equal(resign.profileProblem(profile, 'UDID-1', 'visionOS'), null);
  assert.match(resign.profileProblem({ ...profile, expired: true }, 'UDID-1', 'visionOS'), /expired/);
  assert.match(resign.profileProblem(profile, 'UDID-2', 'visionOS'), /doesn't list this device/);
  assert.match(resign.profileProblem({ ...profile, platforms: ['tvOS'] }, 'UDID-1', 'visionOS'), /isn't a visionOS profile/);
  assert.equal(resign.profileProblem({ ...profile, devices: [] }, 'UDID-2', 'iOS'), null, 'a distribution profile lists no devices');
});

// A fake device: records each command, answers by rule, and plays the file copies out on disk.
function harness({ signs = [0], backupFails = false, uninstallFails = false, remoteFiles = 3 } = {}) {
  const calls = [];
  let sign = 0;
  const exec = async (cmd, args) => {
    const joined = [cmd, ...args].join(' ');
    calls.push(joined);
    if (cmd === 'build-and-sign') {
      const code = signs[Math.min(sign++, signs.length - 1)];
      return { code, output: code === 0 ? 'Status: installed' : MISMATCH };
    }
    const verb = args.slice(2, 4).join(' ');
    if (verb === 'copy from') {
      if (backupFails) return { code: 1, output: 'not debuggable' };
      const destination = args[args.indexOf('--destination') + 1];
      fs.mkdirSync(path.join(destination, 'Caches'), { recursive: true });
      if (destination.endsWith('Library')) { fs.mkdirSync(path.join(destination, 'Application Support')); fs.writeFileSync(path.join(destination, 'Application Support', 'a.bin'), 'x'); }
      return { code: 0, output: '' };
    }
    if (verb === 'uninstall app') return { code: uninstallFails ? 1 : 0, output: '' };
    if (verb === 'info files') {
      const at = args.indexOf('--json-output');
      const out = at >= 0 ? args[at + 1] : null;
      if (out) fs.writeFileSync(out, JSON.stringify({ result: { files: Array.from({ length: remoteFiles }, (_, i) => ({ relativePath: `f${i}` })) } }));
      return { code: 0, output: '' };
    }
    return { code: 0, output: '' };
  };
  const logs = [];
  const backups = fs.mkdtempSync(path.join(os.tmpdir(), 'resign-backups-'));
  const options = { ipa: '/kept/a.ipa', bundleId: 'com.example.app', device: 'AVP', profileFile: '/p/other-dev.mobileprovision',
    buildAndSign: 'build-and-sign', cwd: '/repo', env: {}, exec, log: s => logs.push(s), backups };
  return { calls, logs, options, backups };
}
const verbs = calls => calls.map(c => c.startsWith('build-and-sign') ? 'sign' : c.split(' ').slice(3, 5).join(' '));

test('an update the device accepts is done in place, touching no data', async () => {
  const { calls, options } = harness({ signs: [0] });
  const result = await resign.performResign(options);
  assert.equal(result.mode, 'in-place');
  assert.deepEqual(verbs(calls), ['sign']);
  assert.match(calls[0], /--ipa \/kept\/a\.ipa --device AVP --no-mcp --profile \/p\/other-dev\.mobileprovision/);
});

test('a changed App ID is reinstalled: data off the device first, the app removed only after, data put back', async () => {
  const { calls, options, backups } = harness({ signs: [1, 0] });
  const result = await resign.performResign(options);
  assert.equal(result.mode, 'reinstall');
  assert.ok(result.backup.startsWith(backups));
  const order = verbs(calls);
  assert.deepEqual(order.slice(0, 5), ['sign', 'copy from', 'copy from', 'uninstall app', 'sign']);
  assert.ok(order.indexOf('uninstall app') > order.lastIndexOf('copy from'), 'the app is removed only after both backups');
  const puts = calls.filter(c => c.includes('copy to'));
  assert.ok(puts.some(c => c.endsWith('--destination Library/Application Support')));
  assert.ok(!puts.some(c => c.includes('Library/Caches')), 'caches are not restored');
});

test('no backup, no uninstall: a failed backup leaves the app exactly as it was', async () => {
  const { calls, options } = harness({ signs: [1], backupFails: true });
  await assert.rejects(resign.performResign(options), /couldn't back up Documents.*nothing was removed/);
  assert.ok(!calls.some(c => c.includes('uninstall')));
});

test('a signing failure that is not the App ID refusal changes nothing', async () => {
  const { calls, options } = harness();
  options.exec = async (cmd, args) => { calls.push([cmd, ...args].join(' ')); return { code: 1, output: 'No signing certificate' }; };
  await assert.rejects(resign.performResign(options), /app was not changed/);
  assert.deepEqual(calls.length, 1);
});

test('if the new signature will not install, the previous one comes back with its data', async () => {
  const { calls, options, logs } = harness({ signs: [1, 1, 0] });
  await assert.rejects(resign.performResign(options), /previous one was put back/);
  const order = verbs(calls);
  assert.deepEqual(order.slice(0, 6), ['sign', 'copy from', 'copy from', 'uninstall app', 'sign', 'sign']);
  assert.ok(!calls[order.lastIndexOf('sign')].includes('--profile'), 'the recovery install uses the project\'s own profile');
  assert.ok(calls.some(c => c.includes('copy to')), 'and the data is restored');
  assert.match(logs.join(''), /Putting the previous one back/);
});

test('when even the recovery fails the data stays on disk and the error says where', async () => {
  const { options, backups } = harness({ signs: [1, 1, 1] });
  await assert.rejects(resign.performResign(options), error => error.message.includes("couldn't be put back either") && error.message.includes(backups));
});

test('too few restored files is an error that names the backup', async () => {
  const { options } = harness({ signs: [1, 0], remoteFiles: 0 });
  await assert.rejects(resign.performResign(options), /files came back; the backup is at/);
});

test('discarding data skips the backup and the restore', async () => {
  const { calls, options } = harness({ signs: [1, 0] });
  const result = await resign.performResign({ ...options, dataMode: 'discard' });
  assert.equal(result.backup, null);
  assert.deepEqual(verbs(calls), ['sign', 'uninstall app', 'sign']);
});

test('a dry run only checks the data can be reached', async () => {
  const { calls, options } = harness();
  const result = await resign.performResign({ ...options, dryRun: true });
  assert.equal(result.mode, 'dry-run');
  assert.equal(result.dataReadable, true);
  assert.deepEqual(verbs(calls), ['info files']);
});
