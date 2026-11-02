'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');

class RegistrationError extends Error {
  constructor(message, status = 400) { super(message); this.status = status; }
}
const fail = message => { throw new RegistrationError(message); };
const text = (value, max) => typeof value === 'string' && value.length > 0 && value.length <= max && !/[\x00-\x1f\x7f]/.test(value);
function canonical(value) {
  try { return fs.realpathSync(value); } catch { return path.resolve(value); }
}
function validate(input, existing) {
  if (!input || typeof input !== 'object' || Array.isArray(input)) fail('Enter a project and at least one target.');
  if (Object.keys(input).some(key => !['name', 'path', 'branches', 'platforms'].includes(key))) fail('Unknown project field.');
  if (!text(input.name, 80) || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(input.name)) fail('Use a project name with letters, numbers, dots, underscores or hyphens.');
  if (!text(input.path, 4096)) fail('Enter the checkout path on the Mac.');
  const requested = input.path.startsWith('~/') ? path.join(os.homedir(), input.path.slice(2)) : input.path;
  if (!path.isAbsolute(requested)) fail('Use an absolute checkout path on the Mac.');
  let directory;
  try { directory = fs.realpathSync(requested); if (!fs.statSync(directory).isDirectory()) throw new Error(); }
  catch { fail('The checkout directory does not exist on the Mac.'); }
  const git = args => spawnSync('git', ['-C', directory, ...args], { encoding: 'utf8', timeout: 10000, maxBuffer: 1024 * 1024 });
  const top = git(['rev-parse', '--show-toplevel']);
  if (top.status !== 0 || canonical(top.stdout.trim()) !== directory) fail('Choose the root of an existing Git checkout.');
  const configs = ['build-signing.conf', 'scripts/build-signing.conf'];
  if (!configs.some(name => { try { return fs.statSync(path.join(directory, name)).isFile(); } catch { return false; } })) {
    fail('Set up build-signing.conf in this checkout before registering it.');
  }
  if (existing.some(repo => repo.name === input.name || canonical(repo.path) === directory)) {
    throw new RegistrationError('That project name or checkout is already registered.', 409);
  }
  if (!Array.isArray(input.platforms) || !input.platforms.length || input.platforms.length > 32) fail('Add between one and 32 targets.');
  const pairs = new Set();
  const platforms = input.platforms.map(target => {
    if (!target || Object.keys(target).some(key => !['platform', 'device', 'bundleId'].includes(key)) ||
        !text(target.platform, 40) || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(target.platform) ||
        !text(target.device, 128) || !text(target.bundleId, 255) ||
        !/^[A-Za-z0-9]+(?:[.-][A-Za-z0-9]+)*$/.test(target.bundleId)) fail('Each target needs a platform alias, device name and bundle identifier.');
    const pair = JSON.stringify([target.platform, target.device]);
    if (pairs.has(pair)) fail('Each platform and device pair must be unique.');
    pairs.add(pair);
    return { platform: target.platform, device: target.device, bundleId: target.bundleId };
  });
  let branches = input.branches;
  if (branches === undefined) {
    const current = git(['symbolic-ref', '--quiet', '--short', 'HEAD']);
    branches = current.status === 0 ? [current.stdout.trim()] : [];
  }
  if (!Array.isArray(branches) || branches.length > 64 || branches.some(branch => !text(branch, 255) || branch === 'current' ||
      git(['show-ref', '--verify', '--quiet', `refs/heads/${branch}`]).status !== 0)) fail('Choose existing local branches; current checkout is always offered separately.');
  return { name: input.name, path: directory, branches: [...new Set(branches)], platforms };
}

// Preserve every unrelated configuration value. Never source a signing config,
// build, install, or contact Apple's services while registering a project.
function registerProject(file, input) {
  const original = fs.readFileSync(file);
  const config = JSON.parse(original);
  const project = validate(input, config.repos || []);
  const next = { ...config, repos: [...(config.repos || []), project] };
  const temporary = file + '.incoming-' + crypto.randomUUID();
  let descriptor;
  try {
    descriptor = fs.openSync(temporary, 'wx', 0o600);
    fs.writeFileSync(descriptor, JSON.stringify(next, null, 2) + '\n'); fs.fsyncSync(descriptor);
    fs.closeSync(descriptor); descriptor = undefined;
    if (!fs.readFileSync(file).equals(original)) throw new RegistrationError('The Mac configuration changed; try registering again.', 409);
    fs.renameSync(temporary, file);
  } finally {
    if (descriptor !== undefined) fs.closeSync(descriptor);
    if (fs.existsSync(temporary)) fs.unlinkSync(temporary);
  }
  return { config: next, project };
}
module.exports = { registerProject, validate, RegistrationError };
