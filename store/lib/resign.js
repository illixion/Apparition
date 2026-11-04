'use strict';
// Re-signing an app that is already on a device, from the exact signed IPA the store kept
// for it (lib/artifacts.js): the one thing the device itself can never give back.
//
// Signing the kept IPA with another profile and installing it over the old copy keeps the
// app's data, as long as installd accepts the update. It refuses one thing: an upgrade whose
// application-identifier differs from the installed app's, which is what switching to another
// explicit App ID's profile changes (MismatchedApplicationIdentifierEntitlement). Then the only
// way is to take the data off the device, remove the app, install the new signature and put
// the data back, so that is what `performResign` does, keeping the backup on disk throughout
// and putting the previous signature back if the new one won't install.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const PROFILE_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/;
// build-and-sign prints only the header of installd's error, which names the check that
// failed; the full sentence ("does not match installed application's ...") stays in its own log.
const MISMATCH = /MismatchedApplicationIdentifierEntitlement|application-identifier entitlement string|_validateApplicationIdentifierForNewBundleSigningInfo/;
// What lives in an app's data container and is worth moving. Caches and tmp are rebuilt by the app.
const DOMAINS = ['Documents', 'Library'];
const SKIPPED = new Set(['Library/Caches', 'tmp']);

function profileDir() {
  return path.join(process.env.BUILD_SIGNING_DIR || path.join(os.homedir(), '.config', 'build-signing'), 'profiles');
}
function backupRoot() {
  return process.env.APPARITION_RESIGN_BACKUPS || path.join(os.homedir(), '.local', 'state', 'apparition', 'resign-backups');
}

// A profile's plist (already decoded) as what the picker and the planner need.
function summarize(id, plist) {
  const entitlements = plist.Entitlements || {};
  const team = (plist.TeamIdentifier || [])[0] || '';
  const full = String(entitlements['application-identifier'] || '');
  return {
    id, name: plist.Name || id, team: plist.TeamName || team, teamId: team,
    appId: team && full.startsWith(team + '.') ? full.slice(team.length + 1) : full,
    expires: plist.ExpirationDate ? new Date(plist.ExpirationDate).toISOString() : null,
    debuggable: entitlements['get-task-allow'] === true,
    platforms: plist.Platform || [], devices: plist.ProvisionedDevices || [],
  };
}

function decodeProfile(file) {
  // plutil can't write JSON while the plist still holds the certificates (data) and dates,
  // so work on a temporary copy with those removed and read the expiry on its own.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'profile-'));
  const plist = path.join(dir, 'profile.plist');
  try {
    if (spawnSync('security', ['cms', '-D', '-i', file, '-o', plist]).status !== 0) return null;
    const expires = spawnSync('plutil', ['-extract', 'ExpirationDate', 'raw', '-o', '-', plist]).stdout.toString().trim();
    spawnSync('/usr/libexec/PlistBuddy', ['-c', 'Delete :DeveloperCertificates', '-c', 'Delete :DER-Encoded-Profile',
      '-c', 'Delete :CreationDate', '-c', 'Delete :ExpirationDate', plist]);
    const json = spawnSync('plutil', ['-convert', 'json', '-o', '-', plist], { maxBuffer: 8 * 1024 * 1024 });
    if (json.status !== 0) return null;
    return { ...JSON.parse(json.stdout.toString()), ExpirationDate: expires || null };
  } catch { return null; }
  finally { fs.rmSync(dir, { recursive: true, force: true }); }
}

// Every .mobileprovision in the build-signing profiles folder, unexpired first. `decode`
// is injectable for tests.
function listProfiles({ dir = profileDir(), decode = decodeProfile, now = Date.now() } = {}) {
  let names = [];
  try { names = fs.readdirSync(dir); } catch { return []; }
  const profiles = [];
  for (const name of names.sort()) {
    const id = name.replace(/\.mobileprovision$/, '');
    if (id === name || !PROFILE_ID.test(id)) continue;
    const plist = decode(path.join(dir, name));
    if (!plist) continue;
    const summary = summarize(id, plist);
    profiles.push({ ...summary, file: path.join(dir, name), expired: !!summary.expires && Date.parse(summary.expires) < now });
  }
  return profiles;
}
// The public view: nothing about the Mac's paths, and device lists reduced to a count.
function publicProfile({ file, devices, ...rest }) { return { ...rest, deviceCount: devices.length }; }

function findProfile(id, options) {
  if (typeof id !== 'string' || !PROFILE_ID.test(id)) return null;
  return listProfiles(options).find(p => p.id === id) || null;
}

// Why a profile can't sign for this device, or null. Pure, so the service can refuse early.
function profileProblem(profile, udid, platform) {
  if (profile.expired) return `${profile.name} has expired.`;
  if (udid && profile.devices.length && !profile.devices.includes(udid)) return `${profile.name} doesn't list this device.`;
  const wanted = { visionos: ['xros', 'visionos'], tvos: ['tvos'], ios: ['ios'], ipad: ['ios'] }[String(platform || '').toLowerCase()];
  if (wanted && profile.platforms.length && !profile.platforms.some(p => wanted.includes(String(p).toLowerCase()))) {
    return `${profile.name} isn't a ${platform} profile.`;
  }
  return null;
}

// How the app's data and the app itself are handled on the device. devicectl goes through
// CoreDevice, whose tunnel makes Xcode's DeviceHub wake a sleeping Apple TV, so a device in
// `quietAppsDevices` goes through Apparition's own tunnel instead (`apparition container`,
// `apparition uninstall`). The signing and installing is build-and-sign's, which already
// installs to those devices with `apparition push` (QUIET_DEVICES).
function deviceOps({ exec, device, bundleId, apparition }) {
  if (apparition) {
    const run = (...args) => exec(apparition, [...args, '--device', device], { timeout: 30 * 60 * 1000 });
    const json = output => { const line = String(output).trim().split('\n').reverse().find(l => l.startsWith('{')); return line ? JSON.parse(line) : null; };
    return {
      probe: async () => (await run('container', 'count', bundleId, '--json')).code === 0,
      backup: async backup => {
        const pulled = await run('container', 'pull', bundleId, backup, '--json');
        if (pulled.code !== 0) throw new Error("couldn't back up the app's data (is the app debuggable?); nothing was removed. Choose to discard data to continue anyway.");
      },
      uninstall: async () => (await run('uninstall', bundleId)).code === 0,
      restore: async backup => {
        const pushed = await run('container', 'push', bundleId, backup, '--json');
        if (pushed.code !== 0) throw new Error(`couldn't put the data back; the backup is at ${backup}`);
      },
      count: async () => {
        const counted = await run('container', 'count', bundleId, '--json');
        const reply = counted.code === 0 ? json(counted.output) : null;
        return reply ? reply.folders + reply.files : 0;
      },
    };
  }
  const devicectl = (...args) => exec('xcrun', ['devicectl', ...args], { timeout: 30 * 60 * 1000 });
  const container = ['--domain-type', 'appDataContainer', '--domain-identifier', bundleId];
  return {
    probe: async () => (await devicectl('device', 'info', 'files', '--device', device, ...container, '--subdirectory', 'Library', '--quiet')).code === 0,
    backup: async backup => {
      for (const domain of DOMAINS) {
        fs.mkdirSync(path.join(backup, domain), { recursive: true });
        const copied = await devicectl('device', 'copy', 'from', '--device', device, ...container, '--source', domain, '--destination', path.join(backup, domain));
        if (copied.code !== 0) throw new Error(`couldn't back up ${domain} (is the app debuggable?); nothing was removed. Choose to discard data to continue anyway.`);
      }
    },
    uninstall: async () => (await devicectl('device', 'uninstall', 'app', '--device', device, bundleId)).code === 0,
    restore: async (backup, readDir) => {
      for (const domain of DOMAINS) {
        for (const entry of readDir(path.join(backup, domain))) {
          const relative = `${domain}/${entry}`;
          if (SKIPPED.has(relative)) continue;
          const copied = await devicectl('device', 'copy', 'to', '--device', device, ...container,
            '--source', path.join(backup, domain, entry), '--destination', relative);
          if (copied.code !== 0) throw new Error(`couldn't put ${relative} back; the backup is at ${backup}`);
        }
      }
    },
    count: () => remoteCount(devicectl, device, container),
  };
}

// exec(cmd, args, {cwd, env, timeout}) -> Promise<{code, output}>; it streams what the command
// prints to the job log too. Everything on the Mac and the device goes through it. `apparition`
// (the CLI's path) routes the device side through Apparition's tunnel; see deviceOps.
async function performResign({ ipa, bundleId, device, profileFile, buildAndSign, cwd, env, dataMode = 'keep', dryRun = false,
  exec, log, now = () => new Date(), backups = backupRoot(), readDir = fs.readdirSync, countLocal = defaultCountLocal, apparition = null }) {
  const ops = deviceOps({ exec, device, bundleId, apparition });
  const sign = async file => {
    const args = ['--ipa', ipa, '--device', device, '--no-mcp'];
    if (file) args.push('--profile', file);
    // The script's path, or [command, ...leading arguments] (`apparition build`).
    const [command, ...leading] = [].concat(buildAndSign);
    return exec(command, [...leading, ...args], { cwd, env: { ...env, BUILD_BUNDLE_ID: bundleId }, timeout: 20 * 60 * 1000 });
  };

  if (dryRun) {
    const readable = await ops.probe();
    log(readable
      ? `==> Dry run: ${bundleId} on ${device} is installed and its data container is readable, so it can be backed up. Nothing was changed.\n`
      : `==> Dry run: ${bundleId} isn't installed on ${device}, or isn't debuggable, so its data can't be backed up first.\n`);
    return { mode: 'dry-run', dataReadable: readable };
  }

  log(`==> Re-signing ${bundleId} on ${device} with ${path.basename(profileFile)}: trying an in-place update first\n`);
  let attempt = await sign(profileFile);
  if (attempt.code === 0) return { mode: 'in-place' };
  if (!MISMATCH.test(attempt.output)) throw new Error('signing or installing failed (see the log above); the app was not changed');

  log(`\n==> The device refuses to update ${bundleId} to another App ID. Reinstalling instead; the data goes off the device first.\n`);
  const keep = dataMode !== 'discard';
  const backup = path.join(backups, `${now().toISOString().replace(/[:.]/g, '-')}-${bundleId}`);
  if (keep) {
    await ops.backup(backup);
    log(`==> Backed up ${bundleId}'s Documents and Library to ${backup}\n`);
  }

  const restore = async () => {
    if (!keep) return;
    await ops.restore(backup, readDir);
    const expected = countLocal(backup, SKIPPED);
    const found = await ops.count();
    log(`==> Restored ${found} of ${expected} files${found < expected ? ' (fewer than were backed up: check the backup)' : ''}\n`);
    if (found < expected) throw new Error(`only ${found} of ${expected} files came back; the backup is at ${backup}`);
  };

  if (!(await ops.uninstall())) throw new Error(`couldn't remove ${bundleId} (${keep ? `the backup is at ${backup}` : 'nothing was backed up'})`);

  attempt = await sign(profileFile);
  if (attempt.code !== 0) {
    log(`\n==> The new signature won't install. Putting the previous one back${keep ? ' with its data' : ''}.\n`);
    const back = await sign(null);
    if (back.code === 0) { try { await restore(); } catch (error) { log(`==> ${error.message}\n`); } }
    throw new Error(`the new signature didn't install; ${back.code === 0 ? 'the previous one was put back' : `the previous one couldn't be put back either${keep ? `; the data is safe at ${backup}` : ''}`}`);
  }
  await restore();
  return { mode: 'reinstall', backup: keep ? backup : null };
}

function defaultCountLocal(backup, skipped) {
  let count = 0;
  const walk = (dir, relative) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const name = `${relative}/${entry.name}`;
      if (skipped.has(name)) continue;
      if (entry.isDirectory()) { count += 1; walk(path.join(dir, entry.name), name); } else count += 1;
    }
  };
  for (const domain of DOMAINS) walk(path.join(backup, domain), domain);
  return count;
}

// How many files and folders the container holds under Documents and Library (minus the skipped
// ones), by asking the device: devicectl writes JSON to a file, not stdout.
async function remoteCount(devicectl, device, container) {
  let total = 0;
  for (const domain of DOMAINS) {
    const out = path.join(os.tmpdir(), `resign-files-${process.pid}-${Date.now()}-${domain}.json`);
    const listed = await devicectl('device', 'info', 'files', '--device', device, ...container, '--subdirectory', domain, '--json-output', out, '--quiet');
    if (listed.code !== 0) continue;
    try {
      const files = JSON.parse(fs.readFileSync(out, 'utf8')).result.files;
      total += files.filter(f => ![...SKIPPED].some(s => `${domain}/${f.relativePath}`.startsWith(s))).length;
    } finally { fs.rmSync(out, { force: true }); }
  }
  return total;
}

module.exports = { listProfiles, publicProfile, findProfile, profileProblem, summarize, performResign, profileDir, backupRoot, MISMATCH };
