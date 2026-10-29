'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawnSync } = require('node:child_process');
const { localPackageRefs } = require('./projects');

const digest = value => crypto.createHash('sha256').update(value).digest('hex');
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { maxBuffer: 128 * 1024 * 1024, ...options });
  if (result.status !== 0) throw new Error(`${command} failed while recording build provenance`);
  return result.stdout;
}
function fileDigest(file) {
  const hash = crypto.createHash('sha256'), buffer = Buffer.allocUnsafe(1024 * 1024);
  const descriptor = fs.openSync(file, 'r');
  let bytes = 0, count;
  try {
    while ((count = fs.readSync(descriptor, buffer, 0, buffer.length, null))) {
      hash.update(buffer.subarray(0, count)); bytes += count;
    }
  } finally { fs.closeSync(descriptor); }
  return { sha256: hash.digest('hex'), bytes };
}
function sourceState(directory) {
  const lookup = spawnSync('git', ['-C', directory, 'rev-parse', '--show-toplevel'], { encoding: 'utf8' });
  if (lookup.status !== 0) {
    const root = fs.realpathSync(directory), hash = crypto.createHash('sha256');
    let files = 0;
    const visit = (dir, relative = '') => {
      for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
        if (['.git', '.build', 'build', 'DerivedData', 'node_modules', 'target', '.DS_Store'].includes(entry.name)) continue;
        const name = relative ? relative + '/' + entry.name : entry.name;
        const file = path.join(dir, entry.name);
        if (entry.isDirectory()) visit(file, name);
        else if (entry.isFile() || entry.isSymbolicLink()) {
          hash.update(name + '\0' + fs.lstatSync(file).mode + '\0');
          hash.update(entry.isSymbolicLink() ? fs.readlinkSync(file) : fileDigest(file).sha256);
          files += 1;
        }
      }
    };
    visit(root);
    return { kind: 'directory', path: root, commit: null, branch: null, dirty: true,
      workingTreeSha256: hash.digest('hex'), files, submodules: [] };
  }
  const root = lookup.stdout.trim();
  const git = args => run('git', ['-C', root, ...args]);
  const revision = spawnSync('git', ['-C', root, 'rev-parse', '--verify', 'HEAD'], { encoding: 'utf8' });
  const status = git(['status', '--porcelain=v1', '-z', '--untracked-files=all']);
  const changes = revision.status === 0 ? git(['diff', '--binary', '--no-ext-diff', 'HEAD', '--']) :
    Buffer.concat([git(['diff', '--binary', '--no-ext-diff', '--cached', '--']), git(['diff', '--binary', '--no-ext-diff', '--'])]);
  const untracked = git(['ls-files', '--others', '--exclude-standard', '-z']).toString().split('\0').filter(Boolean).sort();
  const hash = crypto.createHash('sha256').update(status).update(changes);
  for (const name of untracked) {
    const file = path.join(root, name), stat = fs.lstatSync(file);
    hash.update('\0' + name + '\0' + stat.mode + '\0');
    hash.update(stat.isSymbolicLink() ? fs.readlinkSync(file) : fileDigest(file).sha256);
  }
  const branch = spawnSync('git', ['-C', root, 'symbolic-ref', '-q', '--short', 'HEAD'], { encoding: 'utf8' });
  const gitlinks = git(['ls-files', '--stage', '-z']).toString().split('\0').filter(row => row.startsWith('160000 '))
    .map(row => path.join(root, row.slice(row.indexOf('\t') + 1)));
  return { kind: 'git', path: fs.realpathSync(root), commit: revision.status === 0 ? revision.stdout.trim() : null,
    branch: branch.status === 0 ? branch.stdout.trim() : null, dirty: status.length > 0 || revision.status !== 0,
    workingTreeSha256: hash.digest('hex'), submodules: gitlinks };
}

function resolvedPackages(directory) {
  const files = new Set();
  const visit = (dir, depth) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const file = path.join(dir, entry.name);
      if (entry.name === 'Package.resolved' && entry.isFile()) files.add(file);
      if (entry.isDirectory() && depth < 6 && !entry.name.startsWith('.') &&
          !['build', 'DerivedData', 'node_modules', 'target', 'vendor', 'repos'].includes(entry.name)) visit(file, depth + 1);
    }
  };
  visit(directory, 0);
  return [...files].sort().map(file => ({ path: path.relative(directory, file), ...fileDigest(file) }));
}

function snapshot(project, { argumentsFile, toolchain = true } = {}) {
  const directories = new Set(), roots = new Map(), missing = [];
  const queue = [path.resolve(project)];
  while (queue.length) {
    const directory = queue.shift();
    let canonical;
    try { canonical = fs.realpathSync(directory); } catch { missing.push(directory); continue; }
    if (directories.has(canonical)) continue;
    directories.add(canonical);
    const source = sourceState(canonical);
    if (!roots.has(source.path)) {
      source.resolvedPackages = resolvedPackages(source.path);
      roots.set(source.path, source);
      queue.push(...source.submodules);
    }
    queue.push(...localPackageRefs(canonical));
  }
  const sources = [...roots.values()].sort((a, b) => a.path.localeCompare(b.path));
  const builderFiles = ['bin/build-and-sign', 'bin/build-manifest', 'lib/artifacts.js', 'lib/projects.js'];
  const builder = builderFiles.map(file => ({ file, ...fileDigest(path.join(__dirname, '..', file)) }));
  const result = { scope: 'git-or-directory-and-resolved-packages', builder, sources, missing: missing.sort(), anyDirty: sources.some(source => source.dirty),
    argumentsSha256: argumentsFile ? fileDigest(argumentsFile).sha256 : null };
  if (toolchain) result.toolchain = {
    xcode: run('xcodebuild', ['-version'], { encoding: 'utf8' }).trim(),
    swift: run('xcrun', ['swiftc', '--version'], { encoding: 'utf8' }).trim(),
  };
  result.fingerprint = digest(JSON.stringify(result));
  return result;
}

function readPlist(file) {
  return JSON.parse(run('plutil', ['-convert', 'json', '-o', '-', file], { encoding: 'utf8' }));
}
function uuidIndex(app) {
  const result = {};
  const magic = new Set(['feedface', 'cefaedfe', 'feedfacf', 'cffaedfe', 'cafebabe', 'bebafeca', 'cafebabf', 'bfbafeca']);
  const walk = directory => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) walk(file);
      else if (entry.isFile()) {
        const fd = fs.openSync(file, 'r'), header = Buffer.alloc(4);
        try { fs.readSync(fd, header, 0, 4, 0); } finally { fs.closeSync(fd); }
        if (!magic.has(header.toString('hex'))) continue;
        const output = run('dwarfdump', ['--uuid', file], { encoding: 'utf8' });
        for (const match of output.matchAll(/UUID: ([0-9A-Fa-f-]{36}) \(([^)]+)\)/g)) {
          result[match[1].toUpperCase()] = { arch: match[2], binary: path.relative(app, file), ...fileDigest(file) };
        }
      }
    }
  };
  walk(app);
  return result;
}

function defaultStore() {
  return process.env.APPARITION_BUILD_DIR || path.join(process.env.XDG_STATE_HOME || path.join(os.homedir(), '.local', 'state'), 'apparition', 'builds');
}

function readBuild(buildId, { store = defaultStore(), verify = false } = {}) {
  if (!/^[A-Za-z0-9-]{1,64}$/.test(buildId)) throw new Error('invalid build identity');
  const directory = path.join(store, buildId);
  const manifest = JSON.parse(fs.readFileSync(path.join(directory, 'manifest.json')));
  const artifact = manifest.artifact;
  if (manifest.format !== 'apparition-build/1' || manifest.buildId !== buildId ||
      !artifact || !['ipa', 'app.zip'].includes(artifact.kind) || !/^[a-f0-9]{64}$/.test(artifact.sha256) ||
      artifact.file !== `${artifact.sha256}.${artifact.kind}` || !Number.isSafeInteger(artifact.bytes) || artifact.bytes < 0) {
    throw new Error('invalid build manifest');
  }
  const file = path.join(directory, artifact.file);
  if (fs.statSync(file).size !== artifact.bytes || (verify && fileDigest(file).sha256 !== artifact.sha256)) throw new Error('artifact integrity check failed');
  return { manifest, file };
}

function listBuilds({ store = defaultStore(), limit = 40 } = {}) {
  let directories;
  try { directories = fs.readdirSync(store, { withFileTypes: true }); } catch { return []; }
  const newest = directories.filter(entry => entry.isDirectory() && /^[A-Za-z0-9-]{1,64}$/.test(entry.name))
    .map(entry => ({ id: entry.name, at: fs.statSync(path.join(store, entry.name)).mtimeMs }))
    .sort((a, b) => b.at - a.at);
  const builds = [];
  for (const entry of newest) {
    if (builds.length >= limit) break;
    try {
      const { manifest: m } = readBuild(entry.id, { store });
      builds.push({ buildId: m.buildId, createdAt: m.createdAt, project: m.project,
        platform: m.platform, configuration: m.configuration, app: m.app, artifact: m.artifact,
        source: m.source ? { stable: m.source.stable, anyDirty: m.source.before.anyDirty,
          fingerprint: m.source.before.fingerprint } : null });
    } catch { /* incomplete or corrupt imports do not hide other builds */ }
  }
  return builds;
}

function publish({ app, artifact, project, before, after, buildId, keyId = null, platform,
  configuration, originalBundleId, store = defaultStore(), symbols }) {
  store = path.resolve(store);
  if (!/^[A-Za-z0-9-]{1,64}$/.test(buildId)) throw new Error('invalid build identity');
  const info = readPlist(fs.existsSync(path.join(app, 'Contents', 'Info.plist')) ? path.join(app, 'Contents', 'Info.plist') : path.join(app, 'Info.plist'));
  fs.mkdirSync(store, { recursive: true, mode: 0o700 });
  const stage = fs.mkdtempSync(path.join(store, '.incoming-'));
  try {
    let kind = platform === 'macOS' ? 'app.zip' : 'ipa';
    const stagedArtifact = path.join(stage, 'artifact');
    if (fs.statSync(artifact).isDirectory()) {
      kind = 'app.zip';
      run('ditto', ['-c', '-k', '--keepParent', artifact, stagedArtifact]);
    } else fs.copyFileSync(artifact, stagedArtifact, fs.constants.COPYFILE_FICLONE);
    const identity = fileDigest(stagedArtifact);
    const name = `${identity.sha256}.${kind}`;
    fs.renameSync(stagedArtifact, path.join(stage, name));
    const binaries = uuidIndex(app);
    let symbolSet = null;
    if (symbols && fs.existsSync(symbols)) {
      const record = JSON.parse(fs.readFileSync(symbols));
      if (record.buildId !== buildId || record.bundleId !== info.CFBundleIdentifier) throw new Error('symbol identity does not match the build');
      symbolSet = { path: symbols, uuids: record.uuids };
    }
    const manifest = { format: 'apparition-build/1', buildId, keyId, createdAt: new Date().toISOString(),
      project: path.resolve(project), platform, configuration,
      app: { bundleId: info.CFBundleIdentifier, originalBundleId: originalBundleId || info.CFBundleIdentifier,
        name: info.CFBundleDisplayName || info.CFBundleName, bundleName: path.basename(app),
        minOSVersion: info.MinimumOSVersion || info.LSMinimumSystemVersion || null,
        version: info.CFBundleShortVersionString, build: info.CFBundleVersion },
      artifact: { ...identity, kind, file: name }, binaries, symbols: symbolSet,
      source: before && after ? { before, after, stable: before.fingerprint === after.fingerprint } : null };
    fs.writeFileSync(path.join(stage, 'manifest.json'), JSON.stringify(manifest, null, 2), { mode: 0o600 });
    const destination = path.join(store, buildId);
    if (fs.existsSync(destination)) throw new Error('refusing to replace an existing build manifest');
    fs.renameSync(stage, destination);
    return { manifest, path: path.join(destination, 'manifest.json') };
  } finally { if (fs.existsSync(stage)) fs.rmSync(stage, { recursive: true, force: true }); }
}

module.exports = { fileDigest, sourceState, snapshot, uuidIndex, publish, defaultStore, readBuild, listBuilds };
