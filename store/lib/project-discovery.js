'use strict';
// Finds what a checkout builds, so registering a project needs no hand-written configuration.
// Reads only: `git` and `xcodebuild -list/-showBuildSettings`. It never builds, signs or sources
// a conf. The generated `build-signing.conf` is the device-side minimum (project, scheme, platform,
// device): it holds no signing material and is kept out of Git through the clone's local exclude.
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');

class DiscoveryError extends Error {
  constructor(message, status = 400) { super(message); this.status = status; }
}
const fail = message => { throw new DiscoveryError(message); };

// The aliases the build and install code already uses (see examples/build-signing.conf.example).
const PLATFORM_ALIASES = { iphoneos: 'iOS', xros: 'visionOS', appletvos: 'tvOS' };
const MAX_SCHEMES = 8;

function resolveCheckout(input) {
  if (typeof input !== 'string' || !input || input.length > 4096 || /[\x00-\x1f\x7f]/.test(input)) fail('Enter the checkout path on the Mac.');
  const requested = input.startsWith('~/') ? path.join(os.homedir(), input.slice(2)) : input;
  if (!path.isAbsolute(requested)) fail('Use an absolute checkout path on the Mac.');
  let directory;
  try { directory = fs.realpathSync(requested); if (!fs.statSync(directory).isDirectory()) throw new Error(); }
  catch { fail('The checkout directory does not exist on the Mac.'); }
  return directory;
}

// Workspaces first (they carry the packages), then projects, at the root only. An .xcodeproj inside
// a workspace's own `.xcworkspace` bundle is not a candidate.
function containers(directory) {
  const names = fs.readdirSync(directory);
  const found = names.filter(name => /\.xcworkspace$/.test(name) && !name.startsWith('.'))
    .concat(names.filter(name => /\.xcodeproj$/.test(name) && !name.startsWith('.')));
  // A workspace that only wraps one project of the same name adds nothing.
  return found.filter(name => !(name.endsWith('.xcodeproj') && found.includes(name.replace(/\.xcodeproj$/, '.xcworkspace'))));
}

function xcodebuild(directory, args, timeout) {
  const run = spawnSync('xcodebuild', args, { cwd: directory, encoding: 'utf8', timeout, maxBuffer: 64 * 1024 * 1024 });
  if (run.error?.code === 'ENOENT') fail('Xcode is not installed on this Mac.');
  if (run.status !== 0) return null;
  try { return JSON.parse(run.stdout); } catch { return null; }
}

function discover(input, existing = []) {
  const directory = resolveCheckout(input);
  const git = args => spawnSync('git', ['-C', directory, ...args], { encoding: 'utf8', timeout: 10000 });
  const top = git(['rev-parse', '--show-toplevel']);
  if (top.status !== 0 || fs.realpathSync(top.stdout.trim()) !== directory) fail('Choose the root of an existing Git checkout.');
  const branches = git(['for-each-ref', '--format=%(refname:short)', 'refs/heads']).stdout.split('\n').filter(Boolean);
  const current = git(['symbolic-ref', '--quiet', '--short', 'HEAD']);
  const hasConf = ['build-signing.conf', 'scripts/build-signing.conf']
    .some(name => { try { return fs.statSync(path.join(directory, name)).isFile(); } catch { return false; } });
  const registered = existing.some(repo => { try { return fs.realpathSync(repo.path) === directory; } catch { return repo.path === directory; } });
  const found = containers(directory);
  const reply = { name: path.basename(directory).replace(/[^A-Za-z0-9._-]/g, '-').replace(/^[^A-Za-z0-9]+/, '') || 'Project',
    path: directory, branches, currentBranch: current.status === 0 ? current.stdout.trim() : null, hasConf, registered,
    container: found[0] || null, containers: found, apps: [], note: null };
  if (!found.length) {
    reply.note = fs.existsSync(path.join(directory, 'Package.swift'))
      ? 'This is a Swift package without an Xcode project. Apparition builds apps from an .xcodeproj or .xcworkspace.'
      : 'No Xcode project or workspace at the root of this checkout.';
    return reply;
  }
  const flag = found[0].endsWith('.xcworkspace') ? '-workspace' : '-project';
  const listing = xcodebuild(directory, [flag, found[0], '-list', '-json'], 60000);
  const container = listing?.workspace || listing?.project;
  if (!container) { reply.note = 'Xcode could not read ' + found[0] + '. Open it in Xcode once, then try again.'; return reply; }
  for (const scheme of (container.schemes || []).slice(0, MAX_SCHEMES)) {
    const settings = xcodebuild(directory, [flag, found[0], '-scheme', scheme, '-showBuildSettings', '-json'], 90000);
    // The scheme's first product that is an app (not a framework, test bundle or extension).
    const app = (settings || []).map(entry => entry.buildSettings || {})
      .find(s => s.WRAPPER_EXTENSION === 'app' && s.PRODUCT_BUNDLE_IDENTIFIER);
    if (!app) continue;
    const platforms = [...new Set(String(app.SUPPORTED_PLATFORMS || '').split(/\s+/).map(p => PLATFORM_ALIASES[p]).filter(Boolean))];
    reply.apps.push({ scheme, product: app.FULL_PRODUCT_NAME || scheme + '.app', bundleId: app.PRODUCT_BUNDLE_IDENTIFIER, platforms });
  }
  if (!reply.apps.length) reply.note = 'No scheme in ' + found[0] + ' builds an app.';
  return reply;
}

const shellValue = value => "'" + String(value).replace(/'/g, "'\\''") + "'";
const safeValue = value => typeof value === 'string' && value.length > 0 && value.length <= 255 && !/[\x00-\x1f\x7f]/.test(value);

// Validates what registration may generate: a container at the checkout root and a scheme name.
// Nothing here runs; the values are quoted when written.
function validateGeneration(directory, generate, platforms) {
  if (!generate || typeof generate !== 'object' || Array.isArray(generate) ||
      Object.keys(generate).some(key => !['container', 'scheme'].includes(key))) fail('Choose the project and scheme to build.');
  if (!safeValue(generate.container) || !/^[^/]+\.(xcodeproj|xcworkspace)$/.test(generate.container) ||
      !fs.existsSync(path.join(directory, generate.container))) fail('That Xcode project is not at the root of the checkout.');
  if (!safeValue(generate.scheme)) fail('Choose the scheme to build.');
  return { container: generate.container, scheme: generate.scheme, platform: platforms[0].platform, device: platforms[0].device };
}

function confText(g) {
  return [
    '# Written by Apparition. Holds no signing material: builds kept for device-side signing need none.',
    '# Add TEAM_ID, DEV_P12_PATH and DEV_PROFILE_PATH here only for installs signed on this Mac.',
    `PROJECT_PATH=${shellValue(g.container)}`,
    `SCHEME_NAME=${shellValue(g.scheme)}`,
    '# Respect the caller\'s PLATFORM and DEVICE_NAME (Apparition Store sets them per target).',
    `PLATFORM="\${PLATFORM:-${g.platform}}"`,
    `DEVICE_NAME="\${DEVICE_NAME:-${g.device.replace(/(["\\$`])/g, '\\$1')}}"`,
    '',
  ].join('\n');
}

// Writes the conf and keeps it out of Git locally. Returns a function that undoes both.
function writeConf(directory, g) {
  const file = path.join(directory, 'build-signing.conf');
  fs.writeFileSync(file, confText(g), { flag: 'wx', mode: 0o600 });
  let excludeFile = null, excludeBefore = null;
  try {
    const gitDir = spawnSync('git', ['-C', directory, 'rev-parse', '--git-path', 'info/exclude'], { encoding: 'utf8', timeout: 10000 });
    excludeFile = path.resolve(directory, gitDir.stdout.trim());
    excludeBefore = fs.existsSync(excludeFile) ? fs.readFileSync(excludeFile, 'utf8') : '';
    if (!excludeBefore.split('\n').includes('/build-signing.conf')) {
      fs.mkdirSync(path.dirname(excludeFile), { recursive: true });
      fs.writeFileSync(excludeFile, excludeBefore + (excludeBefore && !excludeBefore.endsWith('\n') ? '\n' : '') + '/build-signing.conf\n');
    }
  } catch { excludeFile = null; }
  return () => {
    try { fs.unlinkSync(file); } catch {}
    if (excludeFile !== null) { try { fs.writeFileSync(excludeFile, excludeBefore); } catch {} }
  };
}

module.exports = { discover, validateGeneration, writeConf, confText, DiscoveryError };
