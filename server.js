#!/usr/bin/env node
// Internal app store — builds + installs dev apps onto devices via
// bin/build-and-sign, tracks what each device has (devicectl inventory +
// its own install ledger with branch@commit). Zero npm dependencies.
'use strict';

const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, execFile } = require('node:child_process');
const { readBuild, listBuilds, defaultStore: defaultBuildStore } = require('./lib/artifacts');
const resignTool = require('./lib/resign');

const ROOT = __dirname;
// APPSTORE_CONFIG and APPSTORE_DATA relocate the config and the runtime data
// (the tests use them); by default both live next to this file.
const CONFIG_PATH = process.env.APPSTORE_CONFIG || path.join(ROOT, 'config.json');
if (!fs.existsSync(CONFIG_PATH)) {
  console.error('config.json not found. Copy config.example.json to config.json and edit it.');
  process.exit(1);
}
let CONFIG = JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf8'));
const buildStore = () => path.resolve(path.dirname(CONFIG_PATH), CONFIG.buildManifestDir || defaultBuildStore());
if (process.argv[2] === 'set-password') {
  require('./lib/set-password').run(CONFIG_PATH);
  return;
}
// buildAndSign defaults to the copy that ships in this repo.
const DEFAULT_BUILD_AND_SIGN = path.join(ROOT, 'bin', 'build-and-sign');
// bundlePrefix may be a single string or an array — normalize to an array so
// repos outside the main namespace (e.g. org.example.*) can still be
// tracked from device inventory.
const normalizePrefixes = p => Array.isArray(p) ? p : [p];
let BUNDLE_PREFIXES = normalizePrefixes(CONFIG.bundlePrefix);

// Watch config.json for changes and reload it
let configReloadTimer = null;
const watchConfig = () => fs.watch(path.dirname(CONFIG_PATH), (eventType, filename) => {
  if (filename && String(filename) !== path.basename(CONFIG_PATH)) return;
  // Debounce rapid writes
  if (configReloadTimer) return;
  configReloadTimer = setTimeout(() => {
    configReloadTimer = null;
    try {
      const newConfig = JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf8'));
      CONFIG = newConfig;
      BUNDLE_PREFIXES = normalizePrefixes(CONFIG.bundlePrefix);
      console.log(`[${new Date().toISOString()}] config.json reloaded`);
    } catch (err) {
      console.error(`[${new Date().toISOString()}] failed to reload config.json:`, err.message);
    }
  }, 500);
});
const DATA_DIR = process.env.APPSTORE_DATA || path.join(ROOT, 'data');
const LOG_DIR = path.join(DATA_DIR, 'logs');
const CLONE_DIR = path.join(DATA_DIR, 'build-clones');
const OTA_DIR = path.join(DATA_DIR, 'ota');
fs.mkdirSync(LOG_DIR, { recursive: true });
fs.mkdirSync(OTA_DIR, { recursive: true });
const STATE_FILE = path.join(DATA_DIR, 'state.json');

// ---------- persistent state ----------
// installs[device][bundleId] = {repo, branch, commit, version, buildNo, platform, at}
// deviceApps[device] = {fetchedAt, error?, apps: {bundleId: {name, version, buildNo}}}
// builds = [{id, repo, branch, platform, device, bundleId, status, ...}] (no logs; logs live in files)
// ota[`${bundleId}|${platform}`] = {slug, repo, branch, commit, name, version, buildNo, size, at, backfilled?}
//   — the durable OTA copy of the last signed IPA built for that row, under data/ota/<slug>.ipa
let state = { installs: {}, deviceApps: {}, devices: null, builds: [], nextBuildId: 1, ota: {} };
try {
  state = { ...state, ...JSON.parse(fs.readFileSync(STATE_FILE, 'utf8')) };
} catch { /* first run */ }
// anything that was mid-flight when the server died is failed now — 'queued'
// jobs die too since the in-memory queue[] that would have run them is gone
for (const b of state.builds) {
  if (b.status === 'running' || b.status === 'queued') {
    b.status = 'failed';
    b.error = 'server restarted during build';
  }
}

let saveTimer = null;
function saveState() {
  if (saveTimer) return;
  saveTimer = setTimeout(() => {
    saveTimer = null;
    const tmp = STATE_FILE + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(state, null, 1));
    fs.renameSync(tmp, STATE_FILE);
  }, 250);
}

// ---------- exec helpers ----------
function run(cmd, args, opts = {}) {
  return new Promise((resolve) => {
    execFile(cmd, args, { maxBuffer: 32 * 1024 * 1024, timeout: opts.timeout || 120000, ...opts },
      (err, stdout, stderr) => resolve({ code: err ? (err.code ?? 1) : 0, stdout: stdout || '', stderr: stderr || '', err }));
  });
}

// ---------- git ----------
const gitCache = new Map(); // path -> {at, info}

// Building a branch other than the repo's currently-checked-out one must
// never touch the main working tree — that tree may be mid-edit (an LLM
// working on it, uncommitted changes on main) while an unrelated branch
// gets built. Each such branch gets its own build directory under
// data/build-clones/, wiped and reseeded fresh on every build via `cp -cR`
// — an APFS copy-on-write clonefile, so it's instant and costs no extra
// disk until the copies diverge. It's disposable and rebuilt every time
// (not kept around and incrementally resynced) because git refuses to
// `fetch` into a ref that's already checked out in a non-bare repo — a
// clone left checked out on `branch` from its last build would then fail
// to fetch that same branch's updates on the next one. Unlike `git
// worktree add` (tracked files only), `cp -cR` brings over *everything*:
// gitignored build-signing.conf (local cert/profile paths) and any
// already-fetched deps under repos/, so a PRE_BUILD_HOOK dependency-setup
// script sees them present and skips re-cloning instead of colliding with
// a half-populated fresh worktree.
function safeSlug(s) {
  return s.replace(/[^A-Za-z0-9._-]+/g, '-');
}
// A clone lives in a slot laid out like the checkout's parent directory, so the
// relative package references the apps make (`../../RAVESDK`) resolve: the slot
// data/build-clones/<repo>/<branch>/ holds the clone as <Repo>/ and, beside it, a
// symlink to each package outside the repo that it builds against, transitively.
// The links point at the real checkouts, which is what a build in the main
// checkout uses too. Before this a clone resolved `../DebugTrace` inside
// data/build-clones/<repo>/, so no clone of a RAVE app could build.
function cloneSlot(repoPath, branch) {
  return path.join(CLONE_DIR, safeSlug(path.basename(repoPath)), safeSlug(branch));
}
function clonePath(repoPath, branch) {
  return path.join(cloneSlot(repoPath, branch), path.basename(repoPath));
}
const { localPackageRefs, siblingPackages } = require('./lib/projects');
// `cp -c` shares the data blocks but still creates an inode per file, so cost
// scales with file count: RegentChat's checkout is 357k files (a 27 GB Rust
// `target/`, an Android client, a 15 GB iOS tree) and took 78 s to clone, nearly
// all of it for directories no iOS build reads. A repo's `cloneExclude` (paths
// relative to the repo root) lists what to leave out; the directories that hold
// them are copied entry by entry so nothing else is skipped. Leave out only what
// the build regenerates or never reads: the PRE_BUILD_HOOK's caches stay.
function normalizeExcludes(list) {
  if (!Array.isArray(list)) return [];
  return list.filter(x => typeof x === 'string')
    .map(x => path.posix.normalize(x.trim()).replace(/^\.?\/+|\/+$/g, ''))
    .filter(x => x && x !== '.' && !x.startsWith('..') && x !== '.git');
}
// Clone `src` to `dst`, whole where nothing beneath it is excluded. Pure given the
// copy function, for test/.
async function cloneTree(src, dst, excludes, copy, rel = '') {
  if (!rel) {
    try {
      if (fs.statSync(path.join(src, '.git')).isDirectory()) excludes = [...excludes, '.git/index.lock'];
    } catch { /* source without a Git directory */ }
  }
  if (!excludes.some(x => x.startsWith(rel ? rel + '/' : ''))) return copy(src, dst);
  fs.mkdirSync(dst, { recursive: true });
  const names = fs.readdirSync(src);
  await Promise.all(names.map(async name => {
    const child = rel ? `${rel}/${name}` : name;
    if (excludes.includes(child)) return;
    await cloneTree(path.join(src, name), path.join(dst, name), excludes, copy, child);
  }));
  try { fs.chmodSync(dst, fs.statSync(src).mode); } catch { /* keep the default */ }
}
// `rev` pins the clone to one commit of `branch` instead of its tip (AltSource
// builds build exactly the commit a client was offered).
async function ensureBuildClone(repoPath, branch, excludes = [], rev = null) {
  const slot = cloneSlot(repoPath, branch);
  const dst = clonePath(repoPath, branch);
  // rmSync removes the package symlinks, never what they point at
  fs.rmSync(slot, { recursive: true, force: true });
  fs.mkdirSync(slot, { recursive: true });
  await cloneTree(repoPath, dst, excludes, async (from, to) => {
    const clone = await run('cp', ['-cR', from, to], { timeout: 300000 });
    if (clone.code !== 0) throw new Error(`cp -cR clone of ${from} failed: ${clone.stderr.trim()}`);
  });
  // bring the clone's target branch up to the main repo's latest local
  // commit — fetch from the main repo (a plain local-path remote) since
  // cp -cR only copied whatever ref state existed at clone time
  // --update-head-ok: the copy may have `branch` checked out already (a clone
  // of the branch the main checkout is on); the reset below fixes the tree.
  const fetch = await run('git', ['-C', dst, 'fetch', '--update-head-ok', repoPath, `+refs/heads/${branch}:refs/heads/${branch}`]);
  if (fetch.code !== 0) throw new Error(`git fetch ${branch} into clone failed: ${fetch.stderr.trim()}`);
  // -f: the clone inherits whatever uncommitted state the main tree had at
  // clone time (cp -cR copies working-tree edits too, not just .git) — that
  // has to be force-discarded to switch branches inside the disposable clone
  const checkout = await run('git', ['-C', dst, 'checkout', '-f', branch]);
  if (checkout.code !== 0) throw new Error(`git checkout ${branch} in clone failed: ${checkout.stderr.trim()}`);
  // safe to reset/clean hard: nothing but the store ever touches this clone.
  // -fd (not -x) leaves gitignored paths alone — repos/, build-signing.conf —
  // while sweeping out untracked leftovers copied from main's dirty state
  // (e.g. new not-yet-`git add`ed source files) that don't belong on this branch
  const reset = await run('git', ['-C', dst, 'reset', '--hard', rev || branch]);
  if (reset.code !== 0) throw new Error(`git reset ${rev || branch} in clone failed: ${reset.stderr.trim()}`);
  const clean = await run('git', ['-C', dst, 'clean', '-fd']);
  if (clean.code !== 0) throw new Error(`git clean in clone failed: ${clean.stderr.trim()}`);
  for (const name of siblingPackages(dst, path.dirname(repoPath))) {
    fs.symlinkSync(path.join(path.dirname(repoPath), name), path.join(slot, name));
  }
  return dst;
}
// A clone is disposable, but it is a whole copy of the repo (a clonefile copy is
// free until it diverges, and a build diverges a lot): left alone they added up
// to tens of GB. Two things keep that bounded:
//  - after a build the clone's DerivedData goes, since the next build of that
//    branch wipes the clone anyway. An --inject build keeps its DerivedData-inject,
//    which `inject` compiles patches against while the app runs;
//  - a clone untouched for CLONE_MAX_AGE_DAYS (config `cloneMaxAgeDays`, default 7)
//    is deleted, never one a build is using right now.
const activeClones = new Set();
function dropCloneDerivedData(dir, keepInject) {
  for (const name of keepInject ? ['DerivedData'] : ['DerivedData', 'DerivedData-inject']) {
    fs.rmSync(path.join(dir, 'build', name), { recursive: true, force: true });
  }
}
// Which clone directories (<root>/<repo>/<branch>) are old enough to delete. Pure, for test/.
function staleClones(entries, now, maxAgeMs, active) {
  return entries.filter(e => !active.has(e.dir) && now - e.mtimeMs > maxAgeMs).map(e => e.dir);
}
function reapBuildClones() {
  try {
    const maxAge = (Number(CONFIG.cloneMaxAgeDays) > 0 ? Number(CONFIG.cloneMaxAgeDays) : 7) * 86400000;
    const entries = [];
    for (const repo of fs.readdirSync(CLONE_DIR)) {
      const repoDir = path.join(CLONE_DIR, repo);
      if (!fs.statSync(repoDir).isDirectory()) continue;
      for (const branch of fs.readdirSync(repoDir)) {
        const dir = path.join(repoDir, branch);
        entries.push({ dir, mtimeMs: fs.statSync(dir).mtimeMs });
      }
    }
    for (const dir of staleClones(entries, Date.now(), maxAge, activeClones)) {
      fs.rmSync(dir, { recursive: true, force: true });
      console.log(`reaped stale build clone ${dir}`);
    }
  } catch { /* no clone dir yet, or a race with a build: try again next time */ }
}

async function gitInfo(repoPath) {
  const hit = gitCache.get(repoPath);
  if (hit && Date.now() - hit.at < 10000) return hit.info;
  const [br, ci, st] = await Promise.all([
    run('git', ['-C', repoPath, 'rev-parse', '--abbrev-ref', 'HEAD']),
    run('git', ['-C', repoPath, 'log', '-1', '--format=%h%x09%s%x09%ct']),
    run('git', ['-C', repoPath, 'status', '--porcelain', '--untracked-files=no']),
  ]);
  const [commit, subject, ctime] = (ci.stdout.trim() || '\t\t').split('\t');
  const info = {
    branch: br.stdout.trim() || '?',
    commit, subject,
    commitTime: ctime ? Number(ctime) * 1000 : null,
    dirty: st.stdout.trim().length > 0,
  };
  gitCache.set(repoPath, { at: Date.now(), info });
  return info;
}

// ---------- devicectl ----------
async function devicectlJson(args, timeout) {
  const tmp = path.join(os.tmpdir(), `appstore-dc-${Date.now()}-${Math.random().toString(36).slice(2)}.json`);
  try {
    const r = await run('xcrun', ['devicectl', ...args, '--json-output', tmp], { timeout });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout || 'devicectl failed').trim().split('\n').pop());
    return JSON.parse(fs.readFileSync(tmp, 'utf8'));
  } finally {
    fs.rmSync(tmp, { force: true });
  }
}

async function refreshDeviceList() {
  const j = await devicectlJson(['list', 'devices'], 60000);
  const devices = (j.result?.devices || []).map(d => ({
    name: d.deviceProperties?.name || d.name,
    model: d.hardwareProperties?.marketingName || '',
    state: d.connectionProperties?.tunnelState || '',
    udid: d.hardwareProperties?.udid || d.identifier,
    platform: d.hardwareProperties?.platform || '',
    // devicectl (Xcode 27) lists every simulator too; nothing here installs to one.
    simulated: d.hardwareProperties?.reality === 'simulated',
  })).filter(d => d.name && !d.simulated).map(({ simulated, ...d }) => d);
  state.devices = { fetchedAt: Date.now(), list: devices };
  saveState();
  return devices;
}

const appRefreshInFlight = new Set();
async function refreshDeviceApps(deviceName) {
  if (appRefreshInFlight.has(deviceName)) return;
  appRefreshInFlight.add(deviceName);
  try {
    const j = await devicectlJson(['device', 'info', 'apps', '--device', deviceName], 90000);
    const apps = {};
    for (const a of j.result?.apps || []) {
      if (!BUNDLE_PREFIXES.some(p => a.bundleIdentifier.startsWith(p))) continue;
      apps[a.bundleIdentifier] = { name: a.name, version: a.version, buildNo: a.bundleVersion };
    }
    state.deviceApps[deviceName] = { fetchedAt: Date.now(), apps };
  } catch (e) {
    state.deviceApps[deviceName] = {
      ...(state.deviceApps[deviceName] || { apps: {} }),
      error: String(e.message || e), erroredAt: Date.now(),
    };
  } finally {
    appRefreshInFlight.delete(deviceName);
    saveState();
  }
}

async function refreshAllDevices() {
  try { await refreshDeviceList(); } catch { /* keep cache */ }
  const names = new Set();
  for (const r of CONFIG.repos) for (const p of r.platforms) names.add(p.device);
  await Promise.allSettled([...names].map(refreshDeviceApps));
}

// ---------- build queue (serialized PER REPO; different repos run concurrently) ----------
// build-and-sign writes into $PROJECT_ROOT/build, which is shared by every
// platform of a given repo checkout — two concurrent invocations for the
// SAME repo would stomp each other's DerivedData/IPA output. So each repo
// gets its own queue and runs one build-and-sign at a time; a "deploy to N
// devices" batch fans out into N jobs on that repo's queue that run one
// after another (the UI presents them as a single multi-device deploy even
// though execution is serial per repo). Different repos' queues are
// independent and their builds do run concurrently — that's the whole
// point of a fleet deploy across several apps at once.
//
// build-and-sign also mutates the *login* keychain search list, a resource
// that's global across the whole session, not per repo — so two builds
// from different repos could in principle race that list while both are
// inside their setup/import window. That race is NOT handled by this
// per-repo queue; it's closed on the build-and-sign side instead, via a
// cross-process mutex around exactly that window (bin/build-and-sign,
// SIGNING_LOCK_DIR) — so cross-repo builds still run xcodebuild
// concurrently and only serialize through the short signing tail.
const repoQueues = new Map(); // repo name -> { queue: buildId[], activeId: buildId|null }
function getRepoQueue(repo) {
  let q = repoQueues.get(repo);
  if (!q) { q = { queue: [], activeId: null }; repoQueues.set(repo, q); }
  return q;
}
const liveLogs = new Map(); // buildId -> string (in-memory for this process)
const buildWaiters = new Map(); // buildId -> [resolve] — settled when the build ends, however it ends
function waitForBuild(id) {
  const job = state.builds.find(b => b.id === id);
  if (!job || !['queued', 'running'].includes(job.status)) return Promise.resolve(job);
  return new Promise(resolve => {
    if (!buildWaiters.has(id)) buildWaiters.set(id, []);
    buildWaiters.get(id).push(resolve);
  });
}
function settleWaiters(id) {
  const list = buildWaiters.get(id);
  buildWaiters.delete(id);
  const job = state.builds.find(b => b.id === id);
  for (const fn of list || []) fn(job);
}
const procs = new Map();    // buildId -> ChildProcess (NEVER stored on the job: jobs are JSON-serialized)

function pump(repo) {
  const q = getRepoQueue(repo);
  if (q.activeId != null) return;
  const id = q.queue.shift();
  if (id == null) return;
  const job = state.builds.find(b => b.id === id);
  if (!job || job.status === 'cancelled') { settleWaiters(id); pump(repo); return; }
  q.activeId = id;
  job.status = 'running';
  job.startedAt = Date.now();
  saveState();
  (job.resign ? executeResign(job) : executeBuild(job)).finally(() => { q.activeId = null; settleWaiters(id); pump(repo); });
}

function logPath(id) { return path.join(LOG_DIR, `build-${id}.log`); }

// Extra xcodebuild settings for a build, passed to build-and-sign as repeatable
// `--set KEY=VALUE` (they land AFTER the repo's build-signing.conf on the
// xcodebuild line, so a repeated key overrides the conf). `buildSettings` in
// config.json may be:
//   - an array of "KEY=VALUE" strings         → applied to every branch
//   - an object { branch: [...], "*": [...] } → per-branch, "*" is the default
//   - either form's value may itself be an object { platform: [...], "*": [...] }
//     → per-platform within that branch
// Per-branch matters when a flag only compiles on some branches (its sources
// live only on a feature branch). Per-platform matters when a repo ships one
// platform's client as its own target that links different dependencies, so a
// flag for a dependency that target lacks fails to compile instead of being
// ignored.
// Because these override the conf, a platform-blind setting here cannot be
// undone by the repo conf; it has to be scoped at this level.
function resolveBuildSettings(rc, branch, platform) {
  const bs = rc.buildSettings;
  if (!bs) return [];
  const forBranch = Array.isArray(bs) ? bs : (bs[branch] || bs['*'] || []);
  if (Array.isArray(forBranch)) return forBranch;
  return forBranch[platform] || forBranch['*'] || [];
}

// targets: [{device, branch}] — one job per target, all tagged with a shared
// batchId when there's more than one so the UI can group them as a single
// deploy. Branch is per-target (not one shared value) because a repo can map
// specific devices to specific branches (config.json `deviceBranches`) —
// e.g. the headset builds from a feature branch but the phone from main — and
// those can be deployed together in one batch.
// `device`/`branch` (singular) are kept as an alias for a one-target batch.
// launch = launch after install, the only thing that launches; mcp = make that
// launch a debug session (build-and-sign --run --mcp) instead of a bare one (the
// store's own devicectl launch). mcp without launch installs only, like
// build-and-sign --mcp without --run; buildDebug = --debug
// (Debug configuration); inject = --inject (patchable build)
function enqueueBuild({ repo, targets, device, branch, run: launch, mcp, buildDebug, inject, deploy }) {
  const rc = CONFIG.repos.find(r => r.name === repo);
  if (!rc) throw new Error(`unknown repo: ${repo}`);
  if (!rc.platforms?.length) throw new Error(`${repo} has no device platforms to deploy to`);
  const targetList = (targets && targets.length) ? targets : [{ device: device || rc.platforms[0].device, branch }];
  const jobs = targetList.map(t => {
    const pc = rc.platforms.find(p => p.device === t.device && (!t.platform || p.platform === t.platform)) || rc.platforms[0];
    return {
      id: state.nextBuildId++,
      batchId: null,
      repo, branch: t.branch || 'current',
      platform: pc.platform, device: pc.device, bundleId: pc.bundleId,
      // deploy:false = OTA-only build — build-and-sign gets --no-deploy so an
      // unreachable device can't fail the build; the run/launch step and the
      // install ledger are skipped too (nothing landed on the device)
      noDeploy: deploy === false,
      mcp: !!launch && !!mcp && deploy !== false,
      run: !!launch && !mcp && deploy !== false,
      buildDebug: !!buildDebug,
      inject: !!inject,
      status: 'queued', queuedAt: Date.now(),
    };
  });
  const batchId = jobs.length > 1 ? jobs[0].id : null;
  const q = getRepoQueue(repo);
  for (const j of jobs) {
    j.batchId = batchId;
    state.builds.push(j);
    q.queue.push(j.id);
  }
  if (state.builds.length > 200) state.builds = state.builds.slice(-200);
  saveState();
  pump(repo);
  return jobs;
}

// A build for the AltSource feed: exactly `commit` of the app's source branch,
// from a clean build clone even when that branch is checked out (a dirty tree
// would stamp `-dirty` and never match the listing), signed but not deployed,
// and kept out of the install ledger — whoever downloads it installs it.
function enqueueAltSourceBuild(app, commit) {
  const job = {
    id: state.nextBuildId++, batchId: null,
    repo: app.repo, branch: app.branch, rev: commit, altSource: app.slug,
    platform: app.platform, device: app.device, bundleId: app.bundleId,
    noDeploy: true, mcp: false, run: false, buildDebug: false, inject: false,
    status: 'queued', queuedAt: Date.now(),
  };
  state.builds.push(job);
  if (state.builds.length > 200) state.builds = state.builds.slice(-200);
  getRepoQueue(app.repo).queue.push(job.id);
  saveState();
  pump(app.repo);
  return job;
}

// A Mac app's download build: like an AltSource build, exactly `commit` from a
// clean build clone, signed and never deployed (build-and-sign forces
// --no-deploy for macOS anyway). It has no device; the zip is the result.
function enqueueMacBuild(app, commit, branch) {
  const job = {
    id: state.nextBuildId++, batchId: null,
    repo: app.repo, branch, rev: commit, macApp: app.slug,
    platform: app.platform, device: null, bundleId: app.bundleId,
    noDeploy: true, mcp: false, run: false, buildDebug: false, inject: false,
    status: 'queued', queuedAt: Date.now(),
  };
  state.builds.push(job);
  if (state.builds.length > 200) state.builds = state.builds.slice(-200);
  getRepoQueue(app.repo).queue.push(job.id);
  saveState();
  pump(app.repo);
  return job;
}

// The signed .app a macOS build left behind: the path build-and-sign's report
// names ("App path:"), else the newest build/*-macOS.app. Only paths inside the
// build directory count.
function builtMacApp(buildCwd, logText) {
  const buildDir = path.join(buildCwd, 'build');
  const named = [...String(logText || '').matchAll(/^\s*App path:\s+(.+?)\s*$/gm)].pop()?.[1];
  if (named) {
    const fp = path.resolve(buildCwd, named);
    if (fp.startsWith(buildDir + path.sep) && fs.existsSync(path.join(fp, 'Contents', 'Info.plist'))) return fp;
  }
  try {
    return fs.readdirSync(buildDir).filter(f => f.endsWith('-macOS.app'))
      .map(f => ({ fp: path.join(buildDir, f), m: fs.statSync(path.join(buildDir, f)).mtimeMs }))
      .sort((a, b) => b.m - a.m)[0]?.fp || null;
  } catch { return null; }
}

async function executeBuild(job) {
  if (job.status === 'cancelled') return;
  const rc = CONFIG.repos.find(r => r.name === job.repo);
  const repoPath = rc.path;
  const stream = fs.createWriteStream(logPath(job.id));
  liveLogs.set(job.id, '');
  const log = (s) => {
    liveLogs.set(job.id, liveLogs.get(job.id) + s);
    stream.write(s);
  };

  saveState();

  let cloneDir = null;
  try {
    const orig = (await run('git', ['-C', repoPath, 'rev-parse', '--abbrev-ref', 'HEAD'])).stdout.trim();
    let buildCwd = repoPath;
    if (job.altSource || job.macApp || (job.branch !== 'current' && job.branch !== orig)) {
      log(job.altSource ? `==> AltSource build of ${job.branch}@${job.rev} in a clean build clone\n`
        : job.macApp ? `==> Mac download build of ${job.branch}@${job.rev} in a clean build clone\n`
        : `==> Using dedicated build clone for ${job.branch} (main checkout stays on ${orig})\n`);
      cloneDir = clonePath(repoPath, job.branch);
      activeClones.add(path.dirname(cloneDir));
      buildCwd = await ensureBuildClone(repoPath, job.branch, normalizeExcludes(rc.cloneExclude), job.rev || null);
    }
    const head = (await run('git', ['-C', buildCwd, 'log', '-1', '--format=%h %s'])).stdout.trim();
    job.commit = head.split(' ')[0];
    job.builtBranch = job.branch === 'current' ? orig : job.branch;
    job.buildSettings = resolveBuildSettings(rc, job.builtBranch, job.platform);
    log(`==> Building ${job.repo} [${job.builtBranch}@${job.commit}] platform=${job.platform} → ${job.macApp ? '.app.zip' : job.device}\n`);
    if (job.buildSettings.length) log(`    build settings: ${job.buildSettings.join('  ')}\n`);
    log('\n');
    gitCache.delete(buildCwd);

    const buildArgs = [CONFIG.buildAndSign || DEFAULT_BUILD_AND_SIGN];
    if (job.noDeploy) buildArgs.push('--no-deploy');
    // Always say which: build-and-sign may default to a debug session
    // (MCP_DEFAULT=on), and a store-launched deploy must not become one.
    if (job.mcp) buildArgs.push('--run', '--mcp');
    else if (!job.noDeploy) buildArgs.push('--no-mcp');
    if (job.buildDebug) buildArgs.push('--debug');
    if (job.inject) buildArgs.push('--inject');
    for (const s of job.buildSettings) buildArgs.push('--set', s);

    await new Promise((resolve, reject) => {
      // Exec the script directly rather than through `/bin/bash <script>`:
      // that forced macOS's bash 3.2 and silently overrode the script's own
      // `#!/opt/homebrew/bin/bash` shebang, so anything it wrote for bash 5
      // would break only here, under the agent, and not when run by hand.
      const child = spawn(buildArgs[0], buildArgs.slice(1), {
        cwd: buildCwd,
        env: { ...process.env, PLATFORM: job.platform, APPARITION_BUILD_DIR: buildStore() },
        stdio: ['ignore', 'pipe', 'pipe'],
      });
      procs.set(job.id, child);
      child.stdout.on('data', d => log(d.toString()));
      child.stderr.on('data', d => log(d.toString()));
      let settled = false;
      const settle = (fn, arg) => { if (settled) return; settled = true; procs.delete(job.id); fn(arg); };
      child.on('error', e => settle(reject, e));
      // Settle on 'exit' (the process terminated), NOT 'close' (which also
      // waits for every inherited stdio pipe to reach EOF). `xcrun devicectl`
      // spawns long-lived XPC helpers (CoreDeviceService.xpc, remotepairingd)
      // that inherit our stdout/stderr and keep them open indefinitely, so
      // 'close' may never fire even after a successful build+install — which
      // is exactly what left OK installs stuck reporting "running".
      child.on('exit', (code, signal) => {
        // let any final buffered stdout flush before we resolve/reject
        setImmediate(() => {
          if (job.status === 'cancelled') return settle(reject, new Error('cancelled'));
          if (code === 0) return settle(resolve);
          settle(reject, new Error(`build-and-sign exited with ${code ?? `signal ${signal}`}`));
        });
      });
    });

    // The public build identity links this job, its trace credential and retained
    // symbol set without reading the secret-bearing credential from the IPA.
    job.buildId = [...String(liveLogs.get(job.id) || '').matchAll(/^\s*Build ID:\s+([A-Za-z0-9-]+)\s*$/gm)].pop()?.[1] || null;
    const kept = job.buildId ? readBuild(job.buildId, { store: buildStore(), verify: true }) : null;
    if (kept) {
      if (kept.manifest.app.bundleId !== job.bundleId) throw new Error('build manifest bundle identity differs from the requested target');
      job.artifact = kept.manifest.artifact;
      job.source = kept.manifest.source ? { fingerprint: kept.manifest.source.before.fingerprint,
        stable: kept.manifest.source.stable, anyDirty: kept.manifest.source.before.anyDirty } : null;
      if ((job.altSource || job.macApp) && kept.manifest.source?.stable === false) throw new Error('source changed during compilation; refusing to publish this download');
    }

    // A Mac app is done once its zip is kept. Nothing below applies: there is
    // no IPA (a clone's build/ may still hold another platform's, which must not
    // be published as this one), no device and no install ledger.
    if (job.macApp) {
      const appPath = kept ? kept.manifest.app.bundleName : builtMacApp(buildCwd, liveLogs.get(job.id));
      if (!appPath) throw new Error('no signed .app found after the build');
      const entry = await macBuilds.record(job.macApp, job.rev, job.builtBranch, appPath, kept);
      job.version = entry.version;
      job.buildNo = entry.buildNo;
      job.status = 'success';
      log(`\n==> Store: ${entry.bundle} ${entry.version} (${job.builtBranch}@${job.commit}, ${(entry.size / 1048576).toFixed(1)} MB zipped) ready to download\n`);
      return;
    }

    // read the version actually installed from the SIGNED IPA (the staging
    // build/Payload copy predates the git-hash CFBundleVersion stamp)
    if (kept && kept.manifest.artifact.kind !== 'ipa') throw new Error('device build manifest does not describe an IPA');
    let version = null, buildNo = null, ipaPath = null, appName = null;
    try {
      const buildDir = path.join(buildCwd, 'build');
      const ipa = !kept && fs.readdirSync(buildDir).filter(f => f.endsWith('-signed.ipa'))
        .map(f => ({ f, m: fs.statSync(path.join(buildDir, f)).mtimeMs }))
        .sort((a, b) => b.m - a.m)[0].f;
      ipaPath = kept ? kept.file : path.join(buildDir, ipa);
      const info = await readIpaInfo(ipaPath, fs.statSync(ipaPath).mtimeMs);
      version = info.version;
      buildNo = info.buildNo;
      appName = info.name;
    } catch { /* non-fatal */ }
    job.version = version;
    job.buildNo = buildNo;

    // the feed's copy, read for the listing's permissions; without it there's
    // nothing for the waiting download to serve, so the build has failed
    if (job.altSource) {
      if (!ipaPath) throw new Error('no signed IPA found after the build');
      await altSource.record(job.altSource, job.rev, ipaPath);
      log(`==> Store: AltSource has ${job.bundleId} @ ${job.rev} (${buildNo || '?'})\n`);
    }

    // publish a durable OTA copy — the source IPA won't survive: clone build
    // dirs are wiped on that branch's next build, and the main build dir's
    // IPA is overwritten by whichever platform builds next
    if (ipaPath) {
      try {
        const slug = safeSlug(`${job.bundleId}-${job.platform}`);
        const dest = path.join(OTA_DIR, `${slug}.ipa`);
        fs.rmSync(dest, { force: true });
        fs.copyFileSync(ipaPath, dest, fs.constants.COPYFILE_FICLONE); // APFS clonefile where possible
        state.ota[`${job.bundleId}|${job.platform}`] = {
          slug, bundleId: job.bundleId, repo: job.repo, branch: job.builtBranch, commit: job.commit,
          name: appName || job.repo, version, buildNo,
          size: fs.statSync(dest).size, at: Date.now(),
        };
        log(`==> Store: published OTA copy ${slug}.ipa\n`);
      } catch (e) {
        log(`==> Store: WARNING failed to publish OTA copy: ${String(e.message || e)}\n`);
      }
    }

    // record the install ledger entry. A LAN deploy actually reached the
    // device (devicectl exit 0 said so); an OTA build only ever offers the
    // install to whichever device opens the itms-services link, and Apple
    // gives no callback for whether the person confirmed and it completed
    // — so this is an optimistic guess, not a fact. Still worth recording
    // for the remote-dev case (branch/commit tracking while away from the
    // LAN beats nothing), so it's kept but flagged `confirmed: false` for
    // the UI to display distinctly rather than asserting it as installed.
    if (!job.altSource && !job.forResign) {
      state.installs[job.device] = state.installs[job.device] || {};
      state.installs[job.device][job.bundleId] = {
        repo: job.repo, branch: job.builtBranch, commit: job.commit, buildId: job.buildId,
        platform: job.platform, version, buildNo, at: Date.now(),
        confirmed: !job.noDeploy,
      };
    }
    job.status = 'success';
    if (job.altSource) log(`\n==> Store: AltSource build of ${job.bundleId} ${version || ''} (${job.builtBranch}@${job.commit}) ready for download\n`);
    else if (job.forResign) log(`\n==> Store: built ${job.bundleId} ${version || ''} (${job.builtBranch}@${job.commit}) for the re-sign queued after it\n`);
    else log(job.noDeploy
      ? `\n==> Store: OTA-only build of ${job.bundleId} ${version || ''} (${job.builtBranch}@${job.commit}) — recorded as an unconfirmed guess, not deployed to ${job.device}\n`
      : `\n==> Store: recorded install of ${job.bundleId} ${version || ''} (${job.builtBranch}@${job.commit}) on ${job.device}\n`);

    if (job.run) {
      log(`\n==> Store: launching ${job.bundleId} on ${job.device}\n`);
      try {
        const dev = (state.devices?.list || []).find(d => d.name === job.device);
        if (!dev) throw new Error(`device '${job.device}' not found in devicectl inventory — try refreshing devices`);
        await devicectlJson(['device', 'process', 'launch', '--device', dev.udid, '--terminate-existing', job.bundleId], 60000);
        log(`==> Store: launched ${job.bundleId} on ${job.device}\n`);
      } catch (e) {
        log(`==> Store: WARNING failed to launch app: ${String(e.message || e)}\n`);
      }
    }
  } catch (e) {
    if (job.status !== 'cancelled') job.status = 'failed';
    job.error = String(e.message || e);
    log(`\n==> Store: build ${job.status}: ${job.error}\n`);
  } finally {
    if (cloneDir) {
      activeClones.delete(path.dirname(cloneDir));
      try { dropCloneDerivedData(cloneDir, job.inject && job.status === 'success'); } catch { /* best effort */ }
      reapBuildClones();
    }
    job.finishedAt = Date.now();
    saveState();
    stream.end();
    // keep only the last few live logs in memory; files persist
    if (liveLogs.size > 5) liveLogs.delete(liveLogs.keys().next().value);
  }
  if (job.status === 'success' && !job.noDeploy) refreshDeviceApps(job.device); // async, best-effort
}

// Re-signs an app already on a device from the exact IPA the store kept for it (lib/resign.js),
// as a job on that repo's queue: build-and-sign --ipa writes into the repo's build/ like any
// build, so it must not run alongside one. `body` was validated by resignRequest.
//
// Without a kept build (one made before builds were kept, or one pruned since), the job is
// preceded by an ordinary signed-but-not-deployed build of the project, the same build an OTA
// or Apparition download asks for, and re-signs the IPA that build kept. Both jobs go on the
// repo's queue in order, so the build has always finished when the re-sign starts. A dry run
// only probes the device, so it needs no IPA and builds nothing.
function enqueueResign({ rc, target, kept, branch, profile, dataMode, dryRun }) {
  const queue = getRepoQueue(rc.name);
  const jobs = [];
  let after = null;
  if (!kept && !dryRun) {
    const build = {
      id: state.nextBuildId++, batchId: null,
      repo: rc.name, branch: branch || 'current',
      platform: target.platform, device: target.device, bundleId: target.bundleId,
      noDeploy: true, forResign: true, mcp: false, run: false, buildDebug: false, inject: false,
      status: 'queued', queuedAt: Date.now(),
    };
    jobs.push(build);
    after = build.id;
  }
  const job = {
    id: state.nextBuildId++, batchId: null,
    repo: rc.name, branch: 'current',
    platform: target.platform, device: target.device, bundleId: kept ? kept.manifest.app.bundleId : target.bundleId,
    noDeploy: false, mcp: false, run: false, buildDebug: false, inject: false,
    resign: { from: kept ? kept.manifest.buildId : null, after, profile: profile.id, dataMode, dryRun },
    status: 'queued', queuedAt: Date.now(),
  };
  jobs.push(job);
  if (jobs.length > 1) for (const j of jobs) j.batchId = jobs[0].id;
  for (const j of jobs) { state.builds.push(j); queue.queue.push(j.id); }
  if (state.builds.length > 200) state.builds = state.builds.slice(-200);
  saveState();
  pump(rc.name);
  return jobs;
}

// Turns a client's request into a queued job, or throws an Error with a `status` the route
// reports. Nothing here takes a path from the client. Either
//   {buildId, device, profile, dataMode?, dryRun?}         re-sign that kept IPA, or
//   {repo, device, platform?, branch?, profile, ...}      build that project target first.
function resignRequest(body) {
  const fail = (status, message) => Object.assign(new Error(message), { status });
  body = body || {};
  const fromBuild = typeof body.buildId === 'string';
  if ((!fromBuild && typeof body.repo !== 'string') || typeof body.device !== 'string' || typeof body.profile !== 'string' ||
      (body.platform !== undefined && typeof body.platform !== 'string') ||
      (body.branch !== undefined && typeof body.branch !== 'string') ||
      (body.dataMode !== undefined && !['keep', 'discard'].includes(body.dataMode)) ||
      (body.dryRun !== undefined && typeof body.dryRun !== 'boolean')) {
    throw fail(400, 'Choose a kept build or a project, a device and a profile.');
  }
  let kept = null, found, branch;
  if (fromBuild) {
    try { kept = readBuild(body.buildId, { store: buildStore(), verify: true }); }
    catch { throw fail(404, 'That build is not kept, or its files no longer match its manifest.'); }
    if (kept.manifest.artifact.kind !== 'ipa') throw fail(400, 'Only an IPA can be re-signed.');
    found = CONFIG.repos.flatMap(rc => (rc.platforms || []).map(target => ({ rc, target })))
      .find(({ target }) => target.device === body.device && target.bundleId === kept.manifest.app.bundleId &&
        String(target.platform).toLowerCase() === String(kept.manifest.platform).toLowerCase());
    if (!found) throw fail(400, `No configured project installs ${kept.manifest.app.bundleId} on ${body.device}, so there is no signing setup for it.`);
  } else {
    const rc = CONFIG.repos.find(r => r.name === body.repo);
    const target = rc?.platforms?.find(p => p.device === body.device && (!body.platform || p.platform === body.platform));
    if (!target) throw fail(400, `${body.repo} has no target on ${body.device}${body.platform ? ` for ${body.platform}` : ''}.`);
    if (/^macos$/i.test(target.platform)) throw fail(400, 'A Mac app has no device install to re-sign.');
    branch = body.branch || 'current';
    if (branch !== 'current' && !rc.branches?.includes(branch)) throw fail(400, `${branch} is not one of ${rc.name}'s branches.`);
    found = { rc, target };
  }
  const profile = resignTool.findProfile(body.profile);
  if (!profile) throw fail(400, 'That profile is not in the build-signing folder.');
  const udid = (state.devices?.list || []).find(d => d.name === body.device)?.udid;
  const problem = resignTool.profileProblem(profile, udid, found.target.platform);
  if (problem) throw fail(400, problem);
  return { ...found, kept, branch, profile, dataMode: body.dataMode || 'keep', dryRun: body.dryRun === true };
}

async function executeResign(job) {
  if (job.status === 'cancelled') return;
  const rc = CONFIG.repos.find(r => r.name === job.repo);
  const stream = fs.createWriteStream(logPath(job.id));
  liveLogs.set(job.id, '');
  const log = s => { liveLogs.set(job.id, liveLogs.get(job.id) + s); stream.write(s); };
  // Every command streams to the log; the running child is what Cancel stops.
  const exec = (cmd, args, opts = {}) => new Promise(resolve => {
    let output = '';
    const child = spawn(cmd, args, { cwd: opts.cwd, env: opts.env || process.env, stdio: ['ignore', 'pipe', 'pipe'] });
    procs.set(job.id, child);
    const timer = opts.timeout ? setTimeout(() => child.kill('SIGTERM'), opts.timeout) : null;
    const take = d => { const text = d.toString(); output += text; log(text); };
    child.stdout.on('data', take); child.stderr.on('data', take);
    child.on('error', e => { clearTimeout(timer); resolve({ code: 127, output: output + String(e.message) }); });
    // 'exit', not 'close': devicectl's XPC helpers keep inherited pipes open (see executeBuild).
    child.on('exit', code => { clearTimeout(timer); setImmediate(() => resolve({ code: code ?? 1, output })); });
  });
  saveState();
  try {
    if (job.resign.after != null) {
      const build = state.builds.find(b => b.id === job.resign.after);
      if (!build || build.status !== 'success' || !build.buildId) {
        throw new Error(`build #${job.resign.after} ${build?.status === 'cancelled' ? 'was cancelled' : "didn't produce a kept IPA"}, so there is nothing to re-sign; the app was not changed`);
      }
      job.resign.from = build.buildId;
      log(`==> Using build #${build.id} (${build.builtBranch}@${build.commit}), kept as ${build.buildId}\n`);
    }
    const kept = job.resign.from ? readBuild(job.resign.from, { store: buildStore(), verify: true }) : null;
    if (!kept && !job.resign.dryRun) throw new Error('no kept build to re-sign');
    const profile = resignTool.findProfile(job.resign.profile);
    if (!profile) throw new Error('the profile is no longer in the build-signing folder');
    log(`==> Re-signing ${job.bundleId}${kept ? ` (kept build ${job.resign.from})` : ''} for ${job.device} with ${profile.name} [${profile.appId}]\n`);
    const result = await resignTool.performResign({
      ipa: kept?.file, bundleId: job.bundleId, device: job.device, profileFile: profile.file,
      buildAndSign: CONFIG.buildAndSign || DEFAULT_BUILD_AND_SIGN, cwd: rc.path,
      env: { ...process.env, PLATFORM: job.platform, APPARITION_BUILD_DIR: buildStore() },
      dataMode: job.resign.dataMode, dryRun: job.resign.dryRun, exec, log,
    });
    job.resign.result = result.mode;
    if (result.backup) job.resign.backup = result.backup;
    job.buildId = [...String(liveLogs.get(job.id) || '').matchAll(/^\s*Build ID:\s+([A-Za-z0-9-]+)\s*$/gm)].pop()?.[1] || null;
    if (job.status !== 'cancelled') job.status = 'success';
    if (result.mode !== 'dry-run' && job.buildId) {
      const before = state.installs?.[job.device]?.[job.bundleId];
      state.installs[job.device] = state.installs[job.device] || {};
      state.installs[job.device][job.bundleId] = { ...(before || { repo: job.repo, platform: job.platform }),
        buildId: job.buildId, at: Date.now(), confirmed: true, resigned: profile.id };
    }
    log(`\n==> Store: ${result.mode === 'dry-run' ? 'dry run finished' : `re-signed ${job.bundleId} on ${job.device} (${result.mode})`}\n`);
  } catch (e) {
    if (job.status !== 'cancelled') job.status = 'failed';
    job.error = String(e.message || e);
    log(`\n==> Store: re-sign ${job.status}: ${job.error}\n`);
  } finally {
    job.finishedAt = Date.now();
    saveState();
    stream.end();
    if (liveLogs.size > 5) liveLogs.delete(liveLogs.keys().next().value);
  }
  if (job.status === 'success' && !job.resign.dryRun) refreshDeviceApps(job.device);
}

function cancelBuild(id) {
  const job = state.builds.find(b => b.id === id);
  if (!job) return false;
  if (job.status === 'queued') {
    const q = getRepoQueue(job.repo);
    const i = q.queue.indexOf(id);
    if (i !== -1) q.queue.splice(i, 1);
    job.status = 'cancelled';
    saveState();
    settleWaiters(id);
    return true;
  }
  if (job.status === 'running' && procs.has(id)) {
    job.status = 'cancelled';
    procs.get(id).kill('SIGTERM'); // build-and-sign's EXIT trap cleans up its temp keychain
    return true;
  }
  return false;
}

// ---------- OTA (itms-services install for devices devicectl can't reach) ----------
// Serves the newest signed IPAs plus a generated manifest.plist so a device
// anywhere on the tailnet can install builds from Safari: tailscale-serve
// already proxies https://<host>.ts.net → 127.0.0.1:<port> with a publicly
// trusted cert, which itms-services requires (the plain-HTTP LAN origin
// renders the page, but the install links only work over HTTPS).
// The manifest deliberately omits platform-identifier: visionOS rejects
// Xcode-16-style manifests carrying `com.apple.platform.xros` with
// ASDErrorDomain 752 (developer.apple.com/forums/thread/767089), and iOS
// never needed the key.
const ipaInfoCache = new Map(); // `${path}:${mtime}` -> {bundleId, version, buildNo, name}

async function readIpaInfo(ipaPath, mtimeMs) {
  const key = `${ipaPath}:${mtimeMs}`;
  const hit = ipaInfoCache.get(key);
  if (hit) return hit;
  // unzip's glob `*` matches across `/`, so 'Payload/*.app/Info.plist' also
  // hits nested bundles (an embedded watch app) and -p would concatenate
  // several binary plists into garbage plutil can't parse — list the matches
  // and read only the top-level one (fewest path components)
  const ls = await run('unzip', ['-Z1', ipaPath, 'Payload/*.app/Info.plist']);
  const member = ls.stdout.split('\n').filter(Boolean)
    .sort((a, b) => a.split('/').length - b.split('/').length)[0];
  if (!member) throw new Error(`no Info.plist found in ${path.basename(ipaPath)}`);
  const r = await run('/bin/bash', ['-c',
    `unzip -p "${ipaPath}" "${member.replace(/([[\]*?\\])/g, '\\$1')}" | plutil -convert json -o - -- -`]);
  if (r.code !== 0) throw new Error(`could not read Info.plist from ${path.basename(ipaPath)}`);
  const info = JSON.parse(r.stdout);
  const out = {
    bundleId: info.CFBundleIdentifier,
    version: info.CFBundleShortVersionString || '1.0',
    buildNo: info.CFBundleVersion || '',
    name: info.CFBundleDisplayName || info.CFBundleName || path.basename(ipaPath, '.ipa'),
  };
  ipaInfoCache.set(key, out);
  if (ipaInfoCache.size > 100) ipaInfoCache.delete(ipaInfoCache.keys().next().value);
  return out;
}

function otaEntryBySlug(slug) {
  return Object.values(state.ota).find(e => e.slug === slug);
}

// One-time adoption of IPAs that predate the OTA ledger: for each repo
// platform without an entry, take the newest signed IPA on disk whose
// bundle id matches. Imprecise for repos where several platforms share a
// bundle id (the IPA is whichever platform built last — hence
// `backfilled: true` so the UI can flag it); the next real build of each
// row replaces it with an exact copy.
async function backfillOta() {
  for (const rc of CONFIG.repos) {
    const dirs = [path.join(rc.path, 'build')];
    try {
      const cloneRoot = path.join(CLONE_DIR, safeSlug(path.basename(rc.path)));
      for (const b of fs.readdirSync(cloneRoot)) dirs.push(path.join(cloneRoot, b, path.basename(rc.path), 'build'));
    } catch { /* no clones */ }
    const ipas = [];
    for (const dir of dirs) {
      let files = [];
      try { files = fs.readdirSync(dir); } catch { continue; }
      for (const f of files) {
        if (!f.endsWith('-signed.ipa')) continue;
        const fp = path.join(dir, f);
        try { ipas.push({ fp, mtimeMs: fs.statSync(fp).mtimeMs }); } catch { /* raced */ }
      }
    }
    ipas.sort((a, b) => b.mtimeMs - a.mtimeMs);
    for (const pc of rc.platforms) {
      if (state.ota[`${pc.bundleId}|${pc.platform}`]) continue;
      for (const { fp, mtimeMs } of ipas) {
        let info;
        try { info = await readIpaInfo(fp, mtimeMs); } catch { continue; }
        if (info.bundleId !== pc.bundleId) continue;
        try {
          const slug = safeSlug(`${pc.bundleId}-${pc.platform}`);
          const dest = path.join(OTA_DIR, `${slug}.ipa`);
          fs.copyFileSync(fp, dest, fs.constants.COPYFILE_FICLONE);
          state.ota[`${pc.bundleId}|${pc.platform}`] = {
            slug, bundleId: pc.bundleId, repo: rc.name, branch: null,
            commit: (info.buildNo || '').replace(/-dirty$/i, '') || null,
            name: info.name, version: info.version, buildNo: info.buildNo,
            size: fs.statSync(dest).size, at: mtimeMs, backfilled: true,
          };
          saveState();
        } catch { /* unreadable/uncopyable — leave the row empty */ }
        break; // newest bundle-id match decides this row either way
      }
    }
  }
}

function xmlEsc(s) {
  return String(s).replace(/[<>&'"]/g, c => ({ '<': '&lt;', '>': '&gt;', '&': '&amp;', "'": '&apos;', '"': '&quot;' }[c]));
}

function otaManifest(ipaUrl, info) {
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>items</key>
  <array>
    <dict>
      <key>assets</key>
      <array>
        <dict>
          <key>kind</key><string>software-package</string>
          <key>url</key><string>${xmlEsc(ipaUrl)}</string>
        </dict>
      </array>
      <key>metadata</key>
      <dict>
        <key>bundle-identifier</key><string>${xmlEsc(info.bundleId)}</string>
        <key>bundle-version</key><string>${xmlEsc(info.version)}</string>
        <key>kind</key><string>software</string>
        <key>title</key><string>${xmlEsc(info.name)}</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
`;
}

// The scheme/host the *device* used to reach us — behind tailscale serve
// that's https + the ts.net name (X-Forwarded-*); direct LAN hits fall back
// to the plain Host header.
function requestBase(req) {
  const proto = req.headers['x-forwarded-proto'] || 'http';
  const host = req.headers['x-forwarded-host'] || req.headers.host;
  return `${proto}://${host}`;
}

// A link installd can fetch without the session cookie, valid for an hour.
function signedOtaUrl(req, pathname) {
  return `${requestBase(req)}${pathname}?t=${auth.signPath(pathname)}`;
}

// ---------- AltSource (an AltStore-format feed that builds on download) ----------
// config.json `altSource`: {enabled, name, identifier, subtitle, iconURL, website,
// developerName, branch, token, holdSeconds (50), linkTtlDays (7)}; per repo
// `altSource: false` leaves it out, or {branch, platforms, description, subtitle,
// iconURL, developerName, category}. See lib/altsource.js for the flow.
const { createAltSource, sourceApps, sourcePlatforms, allSourceApps } = require('./lib/altsource');
const altSourceOn = () => CONFIG.altSource?.enabled === true;
const headCache = new Map(); // `${path}|${branch}` -> {at, head}
async function branchHead(app) {
  const key = `${app.path}|${app.branch}`;
  const hit = headCache.get(key);
  if (hit && Date.now() - hit.at < 10000) return hit.head;
  const r = await run('git', ['-C', app.path, 'log', '-1', '--format=%h%x09%cI%x09%s', `refs/heads/${app.branch}`]);
  if (r.code !== 0) throw new Error(r.stderr.trim() || `no branch ${app.branch}`);
  const [commit, date, subject] = r.stdout.trim().split('\t');
  const head = { commit, date, subject };
  headCache.set(key, { at: Date.now(), head });
  return head;
}
const altSource = createAltSource({
  dir: path.join(DATA_DIR, 'altsource'),
  apps: platform => sourceApps(CONFIG, platform),
  allApps: () => allSourceApps(CONFIG),
  head: branchHead,
  enqueue: enqueueAltSourceBuild,
  job: id => state.builds.find(b => b.id === id),
  waitFor: waitForBuild,
  log: m => console.log(m),
});
// A clean LAN or OTA build already stamped with exactly this commit is the same app.
function adoptOtaIpa(app, commit) {
  const e = state.ota[`${app.bundleId}|${app.platform}`];
  if (!e || e.backfilled || !e.buildNo || e.buildNo !== commit) return null;
  const fp = path.join(OTA_DIR, `${e.slug}.ipa`);
  return fs.existsSync(fp) ? fp : null;
}
// The source URL is added to a client once, so it carries a lasting token:
// `altSource.token` when set, else one derived from data/secret.
function altSourceToken() {
  const t = CONFIG.altSource?.token;
  return typeof t === 'string' && t.length >= 16 ? t : auth.derive('altsource-feed');
}
// One source per configured platform name (`/altsource/<platform>.json`), so
// AltStore on an iPhone lists only what the iPhone rows build; `source.json`
// combines them (one row per bundle id) for clients that filter by the
// per-app `platform` key themselves.
function altSourceUrl(req, platform = null) {
  return `${requestBase(req)}/altsource/${platform ? encodeURIComponent(platform) : 'source'}.json?k=${encodeURIComponent(altSourceToken())}`;
}
function altSourceLinks(req) {
  const one = (platform, url) => ({ platform, url,
    altstore: `altstore://source?url=${encodeURIComponent(url)}`, sidestore: `sidestore://source?url=${encodeURIComponent(url)}` });
  return {
    sources: sourcePlatforms(CONFIG).map(p => one(p, altSourceUrl(req, p))),
    combined: one(null, altSourceUrl(req)),
  };
}
// Links inside the feed are signed per path, valid for linkTtlDays (the feed is
// refetched on every refresh, so they never get old in practice).
function altSourceLink(req, pathname) {
  const days = Number(CONFIG.altSource?.linkTtlDays) > 0 ? Number(CONFIG.altSource.linkTtlDays) : 7;
  return `${requestBase(req)}${pathname}?t=${auth.signPath(pathname, days * 86400)}`;
}
function sendText(res, status, text, headers = {}) {
  res.writeHead(status, { 'Content-Type': 'text/plain; charset=utf-8', 'X-Content-Type-Options': 'nosniff', 'Cache-Control': 'no-store', ...headers });
  res.end(text + '\n');
}
function sendIpa(res, fp, name) {
  res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': fs.statSync(fp).size,
    'Content-Disposition': `attachment; filename="${safeSlug(name)}.ipa"`, 'Cache-Control': 'no-store' });
  fs.createReadStream(fp).pipe(res);
}
// The long poll. Built: the IPA. Building: hold the request up to `hold` s
// (`?hold=` or altSource.holdSeconds, default 50: under URLSession's 60 s idle
// timeout), then 307 back to the same URL so the client asks again. Nothing is
// sent while holding, since any byte would commit the response to the IPA.
async function serveAltSourceIpa(req, res, url, slug, commit) {
  const hold = Math.min(Math.max(Number(url.searchParams.get('hold')) || Number(CONFIG.altSource?.holdSeconds) || 50, 1), 3600);
  let r = await altSource.request(slug, commit, { adopt: adoptOtaIpa });
  if (r.jobId != null) {
    let timer;
    await Promise.race([
      waitForBuild(r.jobId),
      new Promise(resolve => { timer = setTimeout(resolve, hold * 1000); }),
      new Promise(resolve => res.once('close', resolve)),
    ]);
    clearTimeout(timer);
    if (res.destroyed || res.writableEnded) return; // client left; the build carries on
    r = altSource.outcome(slug, commit, r.jobId);
    if (r.jobId != null) {
      const next = new URLSearchParams(url.searchParams);
      next.set('w', String((Number(url.searchParams.get('w')) || 0) + 1));
      return sendText(res, 307, `still building (build ${r.jobId}); follow the redirect to keep waiting`,
        { Location: `${url.pathname}?${next}`, 'Retry-After': '1' });
    }
  }
  if (r.entry) return sendIpa(res, altSource.file(r.entry), `${r.entry.name || slug}-${r.entry.buildNo || commit}`);
  sendText(res, r.status || 500, r.error || 'no IPA');
}

// ---------- Mac apps (a signed .app.zip per repo `macApps` entry, built on download) ----------
// See lib/macapps.js. `/mac/<slug>/<commit>.app.zip` serves or builds that commit,
// long-polling like the AltSource IPA; `/mac/<slug>/latest.app.zip?branch=` resolves
// the branch head and redirects there. Both take the session, or a signed link from
// GET /api/mac/<slug>/link for a Mac that has no session (curl on another machine).
const { createMacBuilds, macApps } = require('./lib/macapps');
const MAC_LINK_TTL_DAYS = 7;
const macBuilds = createMacBuilds({
  dir: path.join(DATA_DIR, 'macapps'),
  apps: () => macApps(CONFIG),
  enqueue: enqueueMacBuild,
  job: id => state.builds.find(b => b.id === id),
  waitFor: waitForBuild,
  log: m => console.log(m),
});
// The branch a Mac download builds: one the repo offers, or the one its main
// checkout has out ('current'); anything else is refused.
async function macBranch(app, wanted) {
  const rc = CONFIG.repos.find(r => r.name === app.repo);
  const b = wanted || app.branch;
  if (b === 'current') {
    const out = (await run('git', ['-C', app.path, 'rev-parse', '--abbrev-ref', 'HEAD'])).stdout.trim();
    if (!out || out === 'HEAD') throw Object.assign(new Error(`${app.repo}'s checkout has no branch checked out; pick one`), { status: 409 });
    return out;
  }
  if (b === app.branch || (rc?.branches || []).includes(b)) return b;
  throw Object.assign(new Error(`${app.repo} does not offer branch ${b}`), { status: 400 });
}
const macZipUrl = (slug, commit, branch) => `/mac/${slug}/${commit}.app.zip?branch=${encodeURIComponent(branch)}`;
// A link for a Mac with no session: `latest` follows the branch, so it stays
// good for MAC_LINK_TTL_DAYS. The branch is part of what is signed.
function signedMacLatest(req, slug, branch) {
  const signed = `/mac/${slug}/latest.app.zip?branch=${encodeURIComponent(branch)}`;
  return `${requestBase(req)}${signed}&t=${auth.signPath(signed, MAC_LINK_TTL_DAYS * 86400)}`;
}
function sendMacZip(res, fp, entry) {
  res.writeHead(200, { 'Content-Type': 'application/zip', 'Content-Length': fs.statSync(fp).size,
    'Content-Disposition': `attachment; filename="${safeSlug(`${entry.bundle.replace(/\.app$/, '')}-${entry.buildNo || entry.commit}`)}.app.zip"`,
    'Cache-Control': 'no-store' });
  fs.createReadStream(fp).pipe(res);
}
async function serveMacZip(req, res, url, slug, commit) {
  const app = macBuilds.appBySlug(slug);
  if (!app) return sendText(res, 404, 'unknown Mac app');
  let branch;
  try { branch = await macBranch(app, url.searchParams.get('branch')); } catch (e) { return sendText(res, e.status || 400, e.message); }
  const hold = Math.min(Math.max(Number(url.searchParams.get('hold')) || 50, 1), 3600);
  let r = macBuilds.request(slug, commit, branch);
  if (r.jobId != null) {
    let timer;
    await Promise.race([
      waitForBuild(r.jobId),
      new Promise(resolve => { timer = setTimeout(resolve, hold * 1000); }),
      new Promise(resolve => res.once('close', resolve)),
    ]);
    clearTimeout(timer);
    if (res.destroyed || res.writableEnded) return; // client left; the build carries on
    r = macBuilds.outcome(slug, commit, r.jobId);
    if (r.jobId != null) {
      const next = new URLSearchParams(url.searchParams);
      next.set('w', String((Number(url.searchParams.get('w')) || 0) + 1));
      return sendText(res, 307, `still building (build ${r.jobId}); follow the redirect to keep waiting`,
        { Location: `${url.pathname}?${next}`, 'Retry-After': '1' });
    }
  }
  if (r.entry) return sendMacZip(res, macBuilds.file(r.entry), r.entry);
  sendText(res, r.status || 500, r.error || 'no app');
}
function storeIconFile() {
  return brandIconFile() || path.join(ROOT, 'public', 'apple-touch-icon.png');
}

function otaPage(req) {
  const base = requestBase(req);
  const https = base.startsWith('https://');
  const rows = Object.entries(state.ota)
    .sort((a, b) => b[1].at - a[1].at)
    .map(([key, e]) => {
      const bundleId = e.bundleId || key.split('|')[0];
      const platform = key.split('|')[1];
      const manifestUrl = signedOtaUrl(req, `/ota/${e.slug}/manifest.plist`);
      const itms = `itms-services://?action=download-manifest&url=${encodeURIComponent(manifestUrl)}`;
      const mb = (e.size / 1048576).toFixed(1);
      const when = new Date(e.at).toISOString().replace('T', ' ').slice(0, 16);
      return `<tr>
      <td><b>${xmlEsc(e.name)}</b><br><small>${xmlEsc(bundleId)}</small></td>
      <td>${xmlEsc(e.repo)} · ${xmlEsc(platform)}<br><small>${e.branch ? xmlEsc(e.branch) + '@' : ''}${xmlEsc(e.commit || '?')}${e.backfilled ? ' · pre-existing' : ''}</small></td>
      <td>${mb} MB<br><small>${when}</small></td>
      <td>${https ? `<a class="btn" href="${xmlEsc(itms)}">Install</a>` : '—'}</td>
    </tr>`;
    });
  return `<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>appstore OTA</title>
<style>
  body{font:15px -apple-system,system-ui;background:#111;color:#eee;margin:2em auto;max-width:720px;padding:0 1em}
  table{border-collapse:collapse;width:100%}
  td,th{padding:.6em .5em;border-bottom:1px solid #333;text-align:left;vertical-align:top}
  small{color:#888}
  .btn{display:inline-block;background:#0a84ff;color:#fff;text-decoration:none;padding:.45em 1.1em;border-radius:8px}
  .warn{background:#3a2a00;border:1px solid #7a5c00;padding:.7em 1em;border-radius:8px;margin-bottom:1em}
</style></head><body>
<h2>OTA install</h2>
${https ? '' : `<div class="warn">Opened over plain HTTP — itms-services needs a trusted HTTPS origin. Use the tailnet URL instead.</div>`}
<table><tr><th>App</th><th>Source</th><th>IPA</th><th></th></tr>
${rows.join('\n')}
</table>
<p><small>Latest published build per app/platform. Install requires this device's UDID in the app's provisioning profile.</small></p>
${altSourceOn() ? (() => {
  const { sources, combined } = altSourceLinks(req);
  const block = (label, s) => `<h4>${xmlEsc(label)}</h4>
<p><input readonly style="width:100%;background:#222;color:#eee;border:1px solid #444;padding:.4em;border-radius:6px" value="${xmlEsc(s.url)}" onclick="this.select()"></p>
<p><a class="btn" href="${xmlEsc(s.altstore)}">Add to AltStore</a> <a class="btn" href="${xmlEsc(s.sidestore)}">Add to SideStore</a></p>`;
  return `<h3>AltStore sources</h3>
<p><small>One per platform: add the one for this device. Every app is at the head of its source branch; a download builds that commit if it isn't built yet and answers when the build is done.</small></p>
${sources.map(s => block(s.platform, s)).join('\n')}
${block('All platforms (for clients that filter by the app\'s platform key)', combined)}`;
})() : ''}
</body></html>`;
}

// ---------- debug traces ----------
// Apps built with the DebugTrace package POST trace zips here (the trace
// sheet's Upload button, or a drop on the web UI). build-and-sign gives each
// build its own Ed25519 key: the private half inside the .app, the public
// half in a ledger file under TRACE_KEY_DIR named by key id. A trace is kept
// only when manifest.sig verifies against that build's key, the manifest
// names the same bundle id and build number the ledger recorded, and the zip
// holds exactly the files the manifest lists with matching SHA-256s. The key
// ships in the IPA, so this proves "made by a build this Mac produced, and
// which one", not "sent by a genuine device".
//
// The zip is read here rather than with `unzip`: the writer (DebugTrace's
// ZipWriter) emits plain stored/deflated entries, and reading the central
// directory directly lets every size be capped before anything inflates.
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const TRACE_DIR = path.join(DATA_DIR, 'traces');
const TRACE_KEY_DIR = process.env.DEBUGTRACE_KEY_DIR ||
  path.join(process.env.XDG_STATE_HOME || path.join(os.homedir(), '.local', 'state'), 'debugtrace', 'keys');
const TRACE_MAX_UPLOAD = 64 * 1024 * 1024;
const TRACE_MAX_FILES = 512;
const TRACE_KEEP = 300;
const ED25519_SPKI_PREFIX = Buffer.from('302a300506032b6570032100', 'hex');
fs.mkdirSync(TRACE_DIR, { recursive: true });

const { createAuth, parseCookies } = require('./lib/auth');
const { createDebug } = require('./lib/debug');
const auth = createAuth(DATA_DIR);
const service = require('./lib/service').createService({ auth, config: () => CONFIG, json, readBody,
  registerProject: input => {
    const result = require('./lib/project-registration').registerProject(CONFIG_PATH, input);
    CONFIG = result.config;
    BUNDLE_PREFIXES = normalizePrefixes(CONFIG.bundlePrefix);
    return result.project;
  } });
const debug = createDebug({ tool: () => CONFIG.debugtraceTool, keyDir: TRACE_KEY_DIR, log: m => console.log(m) });

class TraceError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

function readRawBody(req, max) {
  return new Promise((resolve, reject) => {
    const declared = Number(req.headers['content-length'] || 0);
    if (declared > max) return reject(new TraceError(413, `trace is larger than ${max >> 20} MB`));
    const chunks = [];
    let size = 0;
    req.on('data', c => {
      size += c.length;
      if (size > max) { reject(new TraceError(413, `trace is larger than ${max >> 20} MB`)); req.destroy(); return; }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks)));
    req.on('error', reject);
  });
}

// Central directory of a non-zip64 archive → [{name, method, flags, csize, usize, crc, offset}].
function zipEntries(buf) {
  const bad = msg => new TraceError(400, `not a readable trace zip: ${msg}`);
  let eocd = -1;
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 22 - 0xffff); i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) { eocd = i; break; }
  }
  if (eocd < 0) throw bad('no end-of-central-directory record');
  const count = buf.readUInt16LE(eocd + 10);
  const cdSize = buf.readUInt32LE(eocd + 12);
  let p = buf.readUInt32LE(eocd + 16);
  if (count > TRACE_MAX_FILES) throw bad(`${count} entries`);
  if (p + cdSize > eocd) throw bad('central directory out of bounds');
  const entries = [];
  for (let i = 0; i < count; i++) {
    if (p + 46 > eocd || buf.readUInt32LE(p) !== 0x02014b50) throw bad('corrupt central directory');
    const flags = buf.readUInt16LE(p + 8);
    const method = buf.readUInt16LE(p + 10);
    const crc = buf.readUInt32LE(p + 16);
    const csize = buf.readUInt32LE(p + 20);
    const usize = buf.readUInt32LE(p + 24);
    const nlen = buf.readUInt16LE(p + 28), xlen = buf.readUInt16LE(p + 30), clen = buf.readUInt16LE(p + 32);
    const offset = buf.readUInt32LE(p + 42);
    const name = buf.toString('utf8', p + 46, p + 46 + nlen);
    if (flags & 1) throw bad(`${name} is encrypted`);
    if (method !== 0 && method !== 8) throw bad(`${name} uses compression method ${method}`);
    if (csize === 0xffffffff || usize === 0xffffffff || offset === 0xffffffff) throw bad('zip64 is not supported');
    entries.push({ name, method, csize, usize, crc, offset });
    p += 46 + nlen + xlen + clen;
  }
  return entries;
}

function zipRead(buf, entry, expectedBytes) {
  const p = entry.offset;
  if (p + 30 > buf.length || buf.readUInt32LE(p) !== 0x04034b50) throw new TraceError(400, `corrupt entry ${entry.name}`);
  const start = p + 30 + buf.readUInt16LE(p + 26) + buf.readUInt16LE(p + 28);
  if (start + entry.csize > buf.length) throw new TraceError(400, `entry ${entry.name} runs past the end of the zip`);
  const raw = buf.subarray(start, start + entry.csize);
  if (entry.usize !== expectedBytes) throw new TraceError(400, `${entry.name} is ${entry.usize} bytes, the manifest says ${expectedBytes}`);
  if (entry.method === 0) return raw;
  try {
    return zlib.inflateRawSync(raw, { maxOutputLength: Math.max(1, expectedBytes) });
  } catch (e) {
    throw new TraceError(400, `${entry.name} does not inflate to its listed size: ${e.code || e.message}`);
  }
}

// Archive paths as the manifest lists them: relative, no `.`/`..`
// segments, nothing a filesystem would read differently.
function safeTracePath(name) {
  return typeof name === 'string' && name.length > 0 && name.length <= 255 &&
    !name.startsWith('/') && !/[\\\0]/.test(name) &&
    name.split('/').every(seg => seg && seg !== '.' && seg !== '..');
}

function traceKey(keyId) {
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(keyId || '')) return null;
  try { return JSON.parse(fs.readFileSync(path.join(TRACE_KEY_DIR, `${keyId}.json`), 'utf8')); } catch { return null; }
}

// The appstore repo name for a bundle id, when config.json knows it.
function repoForBundle(bundleId) {
  for (const r of CONFIG.repos) {
    if ([...(r.platforms || []), ...(r.macApps || [])].some(p => p?.bundleId === bundleId)) return r.name;
  }
  return null;
}

function verifyTrace(buf) {
  const entries = zipEntries(buf);
  const byName = new Map();
  for (const e of entries) {
    if (!safeTracePath(e.name)) throw new TraceError(400, `unsafe path in zip: ${JSON.stringify(e.name)}`);
    if (byName.has(e.name)) throw new TraceError(400, `duplicate entry ${e.name}`);
    byName.set(e.name, e);
  }
  const manifestEntry = byName.get('manifest.json');
  if (!manifestEntry) throw new TraceError(400, 'no manifest.json — not a DebugTrace archive');
  if (manifestEntry.usize > 4 * 1024 * 1024) throw new TraceError(400, 'manifest.json is implausibly large');
  const manifestBytes = zipRead(buf, manifestEntry, manifestEntry.usize);
  let manifest;
  try { manifest = JSON.parse(manifestBytes.toString('utf8')); } catch { throw new TraceError(400, 'manifest.json is not JSON'); }
  if (manifest.format !== 'debugtrace/1') throw new TraceError(400, `unknown trace format ${JSON.stringify(manifest.format)}`);
  if (!/^[0-9a-f-]{36}$/.test(manifest.traceId || '')) throw new TraceError(400, 'manifest has no valid traceId');
  const sigEntry = byName.get('manifest.sig');
  if (!manifest.signature || !sigEntry) {
    throw new TraceError(403, 'unsigned trace: only apps built by build-and-sign carry a key; share this one as a file instead');
  }
  if (manifest.signature.algorithm !== 'ed25519') throw new TraceError(400, `unsupported signature ${manifest.signature.algorithm}`);
  const key = traceKey(manifest.signature.keyId);
  if (!key) throw new TraceError(403, `signing key ${manifest.signature.keyId} is not in this store's ledger (built on another Mac, or the ledger was cleared)`);
  const publicKey = crypto.createPublicKey({
    key: Buffer.concat([ED25519_SPKI_PREFIX, Buffer.from(key.publicKey, 'base64')]), format: 'der', type: 'spki',
  });
  const sig = zipRead(buf, sigEntry, 64);
  if (!crypto.verify(null, manifestBytes, publicKey, sig)) throw new TraceError(403, 'manifest.sig does not verify');
  if (manifest.app?.bundleId !== key.bundleId) {
    throw new TraceError(403, `trace claims ${manifest.app?.bundleId}, but key ${key.keyId} belongs to ${key.bundleId}`);
  }
  if (key.build && manifest.app?.build !== key.build) {
    throw new TraceError(403, `trace claims build ${manifest.app?.build}, but key ${key.keyId} belongs to build ${key.build}`);
  }
  const listed = Array.isArray(manifest.files) ? manifest.files : [];
  const expected = new Set(['manifest.json', 'manifest.sig']);
  const files = [];
  for (const f of listed) {
    if (!safeTracePath(f.path) || expected.has(f.path)) throw new TraceError(400, `bad manifest entry ${JSON.stringify(f.path)}`);
    expected.add(f.path);
    const entry = byName.get(f.path);
    if (!entry) throw new TraceError(400, `manifest lists ${f.path}, the zip lacks it`);
    const data = zipRead(buf, entry, f.bytes);
    const sha = crypto.createHash('sha256').update(data).digest('hex');
    if (data.length !== f.bytes || sha !== f.sha256) throw new TraceError(403, `${f.path} does not match its manifest hash`);
    files.push({ path: f.path, data });
  }
  for (const name of byName.keys()) {
    if (!expected.has(name)) throw new TraceError(400, `${name} is in the zip but not in the manifest`);
  }
  return { manifest, key, files };
}

function traceMetaPath(id) { return path.join(TRACE_DIR, id, 'meta.json'); }

function listTraces() {
  const out = [];
  for (const id of fs.readdirSync(TRACE_DIR)) {
    try { out.push(JSON.parse(fs.readFileSync(traceMetaPath(id), 'utf8'))); } catch { /* partial or foreign */ }
  }
  return out.sort((a, b) => b.receivedAt - a.receivedAt);
}

function pruneTraces() {
  for (const t of listTraces().slice(TRACE_KEEP)) {
    fs.rmSync(path.join(TRACE_DIR, t.id), { recursive: true, force: true });
  }
}

async function receiveTrace(req) {
  const buf = await readRawBody(req, TRACE_MAX_UPLOAD);
  const { manifest, key, files } = verifyTrace(buf);
  const id = manifest.traceId;
  const dir = path.join(TRACE_DIR, id);
  if (fs.existsSync(traceMetaPath(id))) {
    return { status: 200, meta: { ...JSON.parse(fs.readFileSync(traceMetaPath(id), 'utf8')), duplicate: true } };
  }
  // Written under a temporary name and renamed, so a half-written trace
  // never lists.
  const tmp = path.join(TRACE_DIR, `.incoming-${id}`);
  fs.rmSync(tmp, { recursive: true, force: true });
  fs.mkdirSync(path.join(tmp, 'files'), { recursive: true });
  fs.writeFileSync(path.join(tmp, 'trace.zip'), buf);
  for (const f of files) {
    const dest = path.join(tmp, 'files', f.path);
    fs.mkdirSync(path.dirname(dest), { recursive: true });
    fs.writeFileSync(dest, f.data);
  }
  const meta = {
    id, receivedAt: Date.now(), bytes: buf.length,
    via: req.headers['x-debugtrace-source'] === 'web' ? 'web' : 'app',
    uploader: req.headers['tailscale-user-login'] || null,
    createdAt: manifest.createdAt, session: manifest.session, note: manifest.note || null,
    app: manifest.app, device: manifest.device,
    build: {
      keyId: key.keyId, buildId: key.buildId || key.keyId, repo: repoForBundle(key.bundleId), branch: key.branch || null,
      platform: key.platform || null, configuration: key.configuration || null,
      installedTo: key.device || null, builtAt: key.createdAt || null,
    },
    files: files.map(f => ({ path: f.path, bytes: f.data.length })),
  };
  fs.writeFileSync(path.join(tmp, 'meta.json'), JSON.stringify(meta, null, 1));
  fs.renameSync(tmp, dir);
  pruneTraces();
  setImmediate(linkCrashLogs); // a crash log pulled before this trace arrived
  console.log(`[${new Date().toISOString()}] trace ${id} from ${manifest.app.bundleId} ${manifest.app.build} (${files.length} files, ${buf.length} bytes)`);
  return { status: 201, meta };
}

const TRACE_TEXT = new Set(['.txt', '.json', '.jsonl', '.md', '.log', '.csv']);

// ---------- crash logs from the devices ----------
// The .ips files the devices keep (systemCrashLogs), pulled by a poller; see lib/crashlogs.js.
// config.json `crashLogs`: {enabled (default true), intervalSeconds (300), maxAgeDays (14), backfillDays (3)}.
const { createCrashLogs, crashRefs } = require('./lib/crashlogs');
const crashDevices = {
  async list(device) {
    const j = await devicectlJson(['device', 'info', 'files', '--device', device, '--domain-type', 'systemCrashLogs'], 90000);
    return (j.result?.files || []).map(f => ({
      name: f.relativePath || f.name,
      modified: Date.parse(f.metadata?.lastModDate || '') || 0,
      isDirectory: Boolean(f.resources?.isDirectory),
    }));
  },
  async copy(device, name, dest) {
    const r = await run('xcrun', ['devicectl', 'device', 'copy', 'from', '--device', device,
      '--domain-type', 'systemCrashLogs', '--source', name, '--destination', dest], { timeout: 60000 });
    if (r.code !== 0) throw new Error((r.stderr || r.stdout || 'copy failed').trim().split('\n').pop());
  },
};
// DebugTrace's debugtrace-symbolicate, next to the debugtrace-mcp that `debugtraceTool` names.
function symbolicateTool() {
  if (CONFIG.symbolicateTool) return CONFIG.symbolicateTool;
  return CONFIG.debugtraceTool ? path.join(path.dirname(CONFIG.debugtraceTool), 'debugtrace-symbolicate') : null;
}
const crashLogs = createCrashLogs({
  dir: path.join(DATA_DIR, 'crashlogs'),
  devices: crashDevices,
  prefixes: () => BUNDLE_PREFIXES,
  maxAgeDays: CONFIG.crashLogs?.maxAgeDays || 14,
  backfillDays: CONFIG.crashLogs?.backfillDays || 3,
  log: m => console.log(`[${new Date().toISOString()}] ${m}`),
  async symbolicate(ipsPath) {
    const tool = symbolicateTool();
    if (!tool || !fs.existsSync(tool)) return null;
    const r = await run('/usr/bin/python3', ['-I', tool, ipsPath], { timeout: 60000 });
    return r.code === 0 ? r.stdout : null;
  },
});
function linkCrashLogs() {
  try { crashLogs.link(crashRefs(TRACE_DIR, listTraces().slice(0, 100))); } catch (e) { console.error('crash log link failed:', e.message); }
}
async function pullCrashLogs(force = false) {
  if (CONFIG.crashLogs?.enabled === false) return;
  const names = new Set();
  for (const r of CONFIG.repos) for (const p of r.platforms) names.add(p.device);
  await Promise.allSettled([...names].map(d => crashLogs.pull(d, { force })));
  linkCrashLogs();
}

// ---------- HTTP ----------
function json(res, code, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(code, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
  res.end(body);
}

function publicBuild(b) {
  return b;
}

// Which deploy-menu checkboxes start ticked. config.json's top-level
// `deployDefaults` sets them for every repo; a repo's own `deployDefaults`
// overrides individual keys. Anything not a boolean is ignored.
const DEPLOY_OPTIONS = ['launch', 'mcp', 'buildDebug', 'inject'];
const BUILT_IN_DEPLOY_DEFAULTS = { launch: false, mcp: true, buildDebug: false, inject: false };
function deployDefaults(repo) {
  const out = { ...BUILT_IN_DEPLOY_DEFAULTS };
  for (const src of [CONFIG.deployDefaults, repo.deployDefaults]) {
    for (const k of DEPLOY_OPTIONS) if (typeof src?.[k] === 'boolean') out[k] = src[k];
  }
  return out;
}

async function stateBundle() {
  const repos = await Promise.all(CONFIG.repos.map(async r => ({
    name: r.name, branches: r.branches, platforms: r.platforms,
    deviceBranches: r.deviceBranches || {},
    deployDefaults: deployDefaults(r),
    macApps: macBuilds.list(r.name),
    git: await gitInfo(r.path).catch(e => ({ error: String(e) })),
  })));
  return {
    now: Date.now(),
    host: os.hostname(),
    repos,
    devices: state.devices,
    deviceApps: state.deviceApps,
    installs: state.installs,
    ota: state.ota,
    debugApps: debugAppsCached(),
    authRequired: !!CONFIG.passwordHash,
    builds: state.builds.slice(-40).map(publicBuild).reverse(),
    running: [...repoQueues.values()].map(q => q.activeId).filter(id => id != null),
    queued: [...repoQueues.values()].flatMap(q => q.queue),
  };
}

// Running dev builds, from the last discovery; starts a fresh one when stale
// so /api/state never waits on Bonjour.
let lastDebug = { apps: [], error: null };
function debugAppsCached() {
  debug.discover().then(d => { lastDebug = d; }).catch(() => {});
  return { apps: lastDebug.apps.map(debug.publicApp), error: lastDebug.error };
}

const PUBLIC_FILES = new Set(['/login.html', '/favicon.svg', '/favicon.ico', '/apple-touch-icon.png', '/mask-icon.svg', '/branding/icon']);

// ---------- branding (config.json `branding`) ----------
// { org, title, icon }. `title` is the full name; otherwise "<org>'s App Store";
// otherwise "App Store". `icon` is an image path (relative to config.json) or
// any other text, shown as-is (an emoji, a letter); unset keeps the triangle.
const IMAGE_TYPES = { '.svg': 'image/svg+xml', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg',
  '.gif': 'image/gif', '.webp': 'image/webp', '.ico': 'image/x-icon' };
const escHtml = s => String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
function brandIconFile() {
  const icon = CONFIG.branding?.icon;
  if (typeof icon !== 'string' || !IMAGE_TYPES[path.extname(icon).toLowerCase()]) return null;
  const fp = path.resolve(path.dirname(CONFIG_PATH), icon.replace(/^~(?=\/)/, process.env.HOME || '~'));
  try { return fs.statSync(fp).isFile() ? fp : null; } catch { return null; }
}
function brand() {
  const b = CONFIG.branding || {};
  const title = String(b.title || (b.org ? `${b.org}'s App Store` : 'App Store'));
  // Header text: the org keeps its own case, dim, before the bright uppercase "App Store";
  // a full `title` is shown as written.
  const name = b.title ? `<b>${escHtml(b.title)}</b>`
    : b.org ? `<span class="org">${escHtml(b.org)}&#39;s</span> <b>App Store</b>` : '<b>App Store</b>';
  const file = brandIconFile();
  const icon = typeof b.icon === 'string' ? b.icon.trim() : '';
  let mark = '<span class="tri"></span>', head = '';
  if (file) {
    const href = `/branding/icon?v=${Math.floor(fs.statSync(file).mtimeMs)}`;
    mark = `<img class="brandicon" src="${href}" alt="">`;
    head = `<link rel="icon" href="${href}"><link rel="apple-touch-icon" href="${href}">`;
  } else if (icon) {
    mark = `<span class="brandicon">${escHtml(icon)}</span>`;
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text x="50" y="50" font-size="80" text-anchor="middle" dominant-baseline="central">${escHtml(icon)}</text></svg>`;
    head = `<link rel="icon" href="data:image/svg+xml,${encodeURIComponent(svg)}">`;
  } else {
    head = `<link rel="icon" href="/favicon.svg" type="image/svg+xml"><link rel="alternate icon" href="/favicon.ico" sizes="48x48 32x32 16x16"><link rel="apple-touch-icon" href="/apple-touch-icon.png"><link rel="mask-icon" href="/mask-icon.svg" color="#4f8cff">`;
  }
  return { title, name, mark, head };
}
const isHttps = req => requestBase(req).startsWith('https://');
const sessionToken = req => parseCookies(req.headers.cookie)[auth.COOKIE];
const signedIn = req => !CONFIG.passwordHash || auth.check(sessionToken(req), CONFIG.passwordHash);

// SameSite=Strict already keeps the cookie off other sites' requests; this
// also refuses any state-changing request whose Origin isn't this host.
function crossSite(req) {
  if (req.method === 'GET' || req.method === 'HEAD' || !req.headers.origin) return false;
  const host = req.headers['x-forwarded-host'] || req.headers.host;
  try { return new URL(req.headers.origin).host !== host; } catch { return true; }
}

function sendStatic(res, rel) {
  const fp = path.join(ROOT, 'public', path.normalize(rel));
  if (!fp.startsWith(path.join(ROOT, 'public') + path.sep) || !fs.existsSync(fp) || !fs.statSync(fp).isFile()) return false;
  const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon' };
  res.writeHead(200, { 'Content-Type': types[path.extname(fp)] || 'application/octet-stream',
    'X-Content-Type-Options': 'nosniff', ...(fp.endsWith('.html') ? { 'Cache-Control': 'no-store' } : {}) });
  if (fp.endsWith('.html')) {
    const b = brand();
    return res.end(fs.readFileSync(fp, 'utf8').replace(/\{\{BRAND_(TITLE|NAME|MARK|HEAD)\}\}/g,
      (_, k) => k === 'TITLE' ? escHtml(b.title) : k === 'NAME' ? b.name : k === 'MARK' ? b.mark : b.head)), true;
  }
  res.end(fs.readFileSync(fp));
  return true;
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let data = '';
    req.on('data', c => { data += c; if (data.length > 1e6) req.destroy(); });
    req.on('end', () => { try { resolve(data ? JSON.parse(data) : {}); } catch (e) { reject(e); } });
  });
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://x');
  try {
    if (crossSite(req)) return json(res, 403, { error: 'cross-site request refused' });
    const native = await service(req, res, url);
    if (native.handled) return;
    if (native.path) url.pathname = native.path;

    // ---- reachable without a session ----
    if (url.pathname === '/api/login' && req.method === 'POST') {
      if (!CONFIG.passwordHash) return json(res, 200, { ok: true, open: true });
      const body = await readBody(req).catch(() => ({}));
      const r = auth.login(body.password, CONFIG.passwordHash, { user: req.headers['tailscale-user-login'] || null });
      if (!r.ok) return json(res, r.status, { error: r.error });
      res.setHeader('Set-Cookie', auth.cookie(r.token, isHttps(req)));
      return json(res, 200, { ok: true });
    }
    if (url.pathname === '/api/logout' && req.method === 'POST') {
      auth.logout(sessionToken(req));
      res.setHeader('Set-Cookie', auth.clearCookie(isHttps(req)));
      return json(res, 200, { ok: true });
    }
    if (url.pathname === '/api/auth' && req.method === 'GET') {
      return json(res, 200, { required: !!CONFIG.passwordHash, signedIn: signedIn(req) });
    }
    if (req.method === 'GET' && url.pathname === '/branding/icon') {
      const fp = brandIconFile();
      if (!fp) return json(res, 404, { error: 'no branding icon configured' });
      res.writeHead(200, { 'Content-Type': IMAGE_TYPES[path.extname(fp).toLowerCase()], 'X-Content-Type-Options': 'nosniff',
        // an SVG served from here must not run script of its own
        'Content-Security-Policy': "default-src 'none'; style-src 'unsafe-inline'; img-src data:", 'Cache-Control': 'no-cache' });
      return res.end(fs.readFileSync(fp));
    }
    if (req.method === 'GET' && (url.pathname === '/login' || PUBLIC_FILES.has(url.pathname))) {
      if (url.pathname === '/login' && signedIn(req)) {
        // A same-site path only. Browsers read "/\host" like "//host", and a control
        // character would break the header, so neither is allowed through.
        const next = url.searchParams.get('next') || '/';
        const target = /^\/(?![\/\\])[^\x00-\x1f\x7f]*$/.test(next) ? next : '/';
        res.writeHead(302, { Location: target, 'Cache-Control': 'no-store' });
        return res.end();
      }
      if (sendStatic(res, url.pathname === '/login' ? 'login.html' : url.pathname.slice(1))) return;
    }
    // Apps upload traces with no session; the signature check is the gate.
    const traceUpload = url.pathname === '/api/traces' && req.method === 'POST';
    // installd can't send the cookie, so OTA files also accept a signed ?t=.
    const otaFile = req.method === 'GET' && /^\/ota\/[A-Za-z0-9._-]+(\/manifest\.plist|\.ipa)$/.test(url.pathname)
      && auth.verifyPath(url.pathname, url.searchParams.get('t'));
    // Mac downloads take a signed link too, for a Mac without a session. A
    // `latest` link's signature covers its branch.
    const macPath = req.method === 'GET' && url.pathname.match(/^\/mac\/([A-Za-z0-9._-]+)\/(latest|[0-9a-f]{4,40})\.app\.zip$/);
    const macSigned = !!macPath && url.searchParams.has('t') && auth.verifyPath(macPath[2] === 'latest'
      ? `${url.pathname}?branch=${encodeURIComponent(url.searchParams.get('branch') || '')}` : url.pathname, url.searchParams.get('t'));
    if (macPath && url.searchParams.has('t') && !macSigned && !signedIn(req)) return sendText(res, 403, 'link expired or not signed');

    // AltSource: clients have no cookie. The feed takes its lasting token, its
    // links their own signatures; all of it is gone while the mode is off.
    if (url.pathname.startsWith('/altsource/') && req.method === 'GET') {
      if (!altSourceOn()) return json(res, 404, { error: 'not found' });
      const fm = url.pathname.match(/^\/altsource\/([A-Za-z0-9._-]+)\.json$/);
      if (fm) {
        const k = String(url.searchParams.get('k') || '');
        const want = altSourceToken();
        if (k.length !== want.length || !crypto.timingSafeEqual(Buffer.from(k), Buffer.from(want))) {
          return json(res, 403, { error: 'wrong or missing feed token' });
        }
        let platform = null;
        if (fm[1] !== 'source') {
          platform = sourcePlatforms(CONFIG).find(p => p.toLowerCase() === fm[1].toLowerCase());
          if (!platform) return json(res, 404, { error: `no apps for platform ${fm[1]}`, platforms: sourcePlatforms(CONFIG) });
        }
        const doc = await altSource.source({ title: brand().title, cfg: CONFIG.altSource, platform, url: p => altSourceLink(req, p) });
        res.writeHead(200, { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' });
        return res.end(JSON.stringify(doc, null, 1));
      }
      if (!auth.verifyPath(url.pathname, url.searchParams.get('t'))) return json(res, 403, { error: 'link expired or not signed' });
      if (url.pathname === '/altsource/icon.png') {
        const fp = storeIconFile();
        res.writeHead(200, { 'Content-Type': IMAGE_TYPES[path.extname(fp).toLowerCase()] || 'image/png', 'X-Content-Type-Options': 'nosniff',
          'Content-Security-Policy': "default-src 'none'; style-src 'unsafe-inline'" });
        return res.end(fs.readFileSync(fp));
      }
      let am = url.pathname.match(/^\/altsource\/([A-Za-z0-9._-]+)\/icon\.png$/);
      if (am) {
        const fp = altSource.iconFile(am[1]) || storeIconFile();
        res.writeHead(200, { 'Content-Type': IMAGE_TYPES[path.extname(fp).toLowerCase()] || 'image/png', 'X-Content-Type-Options': 'nosniff',
          'Content-Security-Policy': "default-src 'none'; style-src 'unsafe-inline'" });
        return res.end(fs.readFileSync(fp));
      }
      am = url.pathname.match(/^\/altsource\/([A-Za-z0-9._-]+)\/([0-9a-f]{4,40})\.ipa$/);
      if (am) return serveAltSourceIpa(req, res, url, am[1], am[2]);
      return json(res, 404, { error: 'not found' });
    }

    if (!native.authorized && !traceUpload && !otaFile && !macSigned && !signedIn(req)) {
      if (url.pathname.startsWith('/api/')) return json(res, 401, { error: 'sign in first', login: '/login' });
      res.writeHead(302, { Location: `/login?next=${encodeURIComponent(url.pathname + url.search)}` });
      return res.end();
    }

    // ---- live debugging ----
    if (url.pathname === '/api/debug/apps' && req.method === 'GET') {
      const d = await debug.discover();
      lastDebug = d;
      // clientName is what the device's approval prompt calls this page.
      return json(res, 200, { apps: d.apps.map(debug.publicApp), error: d.error, clientName: debug.clientName });
    }
    let dm = url.pathname.match(/^\/api\/debug\/([A-Za-z0-9._-]{1,64})\/(help|call|logs)$/);
    if (dm) {
      const [, id, action] = dm;
      if (action === 'help' && req.method === 'GET') {
        const r = await debug.help(id);
        return json(res, r.status, r.body);
      }
      if (action === 'call' && req.method === 'POST') {
        const body = await readBody(req).catch(() => null);
        if (!body || typeof body.endpoint !== 'string' || !/^_?[A-Za-z][A-Za-z0-9]{0,47}$/.test(body.endpoint)) {
          return json(res, 400, { ok: false, error: { message: 'body must be {"endpoint": "<name>", "arguments": {...}}' } });
        }
        const r = await debug.call(id, body.endpoint, body.arguments && typeof body.arguments === 'object' ? body.arguments : {});
        if (r.body !== null || !r.raw) return json(res, r.status, r.body);
        // An image or file from the app: passed through, never as HTML.
        const type = /^(image\/(png|jpeg|gif|webp|heic)|application\/(zip|octet-stream)|text\/plain)/.test(r.contentType) ? r.contentType : 'application/octet-stream';
        res.writeHead(r.status, { 'Content-Type': type, 'X-Content-Type-Options': 'nosniff', 'Content-Security-Policy': "default-src 'none'" });
        return res.end(r.raw);
      }
      if (action === 'logs' && req.method === 'GET') {
        const level = url.searchParams.get('level') || 'debug';
        if (!['debug', 'info', 'notice', 'error', 'fault'].includes(level)) return json(res, 400, { error: 'unknown level' });
        return debug.streamLogs(id, req, res, { level, contains: url.searchParams.get('contains') || undefined });
      }
    }
    let om = url.pathname.match(/^\/api\/ota\/([A-Za-z0-9._-]+)\/link$/);
    if (om && req.method === 'GET') {
      const entry = otaEntryBySlug(om[1]);
      if (!entry) return json(res, 404, { error: 'unknown ipa' });
      return json(res, 200, { manifestUrl: signedOtaUrl(req, `/ota/${entry.slug}/manifest.plist`) });
    }

    if (macPath) {
      const [, slug, which] = macPath;
      if (which !== 'latest') return serveMacZip(req, res, url, slug, which);
      const app = macBuilds.appBySlug(slug);
      if (!app) return sendText(res, 404, 'unknown Mac app');
      const branch = await macBranch(app, url.searchParams.get('branch')).catch(e => e);
      if (branch instanceof Error) return sendText(res, branch.status || 400, branch.message);
      const head = await branchHead({ path: app.path, branch });
      let loc = macZipUrl(slug, head.commit, branch);
      if (url.searchParams.has('hold')) loc += `&hold=${encodeURIComponent(url.searchParams.get('hold'))}`;
      if (macSigned) loc += `&t=${auth.signPath(`/mac/${slug}/${head.commit}.app.zip`, MAC_LINK_TTL_DAYS * 86400)}`;
      res.writeHead(302, { Location: loc, 'Cache-Control': 'no-store' });
      return res.end();
    }
    // The page's download button: the zip's URL when that commit is built,
    // else the build it started (or joined) for the page to wait on.
    om = url.pathname.match(/^\/api\/mac\/([A-Za-z0-9._-]+)\/(build|link)$/);
    if (om) {
      const app = macBuilds.appBySlug(om[1]);
      if (!app) return json(res, 404, { error: 'unknown Mac app' });
      const body = om[2] === 'build' && req.method === 'POST' ? await readBody(req).catch(() => ({})) : null;
      const wanted = body ? body.branch : url.searchParams.get('branch');
      const branch = await macBranch(app, typeof wanted === 'string' && wanted ? wanted : null).catch(e => e);
      if (branch instanceof Error) return json(res, branch.status || 400, { error: branch.message });
      if (om[2] === 'link' && req.method === 'GET') {
        const link = signedMacLatest(req, app.slug, branch);
        const zip = `${app.name.replace(/[^A-Za-z0-9._-]+/g, '')}.app.zip`;
        return json(res, 200, { url: link, days: MAC_LINK_TTL_DAYS,
          curl: `curl -fL --max-redirs 100 -o '${zip}' '${link}&hold=600' && ditto -x -k '${zip}' .` });
      }
      if (!body) return json(res, 405, { error: 'POST to build' });
      const head = await branchHead({ path: app.path, branch });
      const r = macBuilds.request(app.slug, head.commit, branch);
      if (r.entry) return json(res, 200, { ready: true, url: macZipUrl(app.slug, r.entry.commit, branch), entry: r.entry, branch });
      if (r.jobId != null) return json(res, 200, { ready: false, jobId: r.jobId, commit: head.commit, branch });
      return json(res, r.status || 500, { error: r.error });
    }

    if (url.pathname === '/api/altsource' && req.method === 'GET') {
      if (!altSourceOn()) return json(res, 200, { enabled: false });
      return json(res, 200, { enabled: true, ...altSourceLinks(req), building: altSource.building() });
    }
    if (url.pathname === '/api/state' && req.method === 'GET') {
      return json(res, 200, await stateBundle());
    }
    if (url.pathname === '/api/resign/options' && req.method === 'GET') {
      return json(res, 200, { profiles: resignTool.listProfiles().map(resignTool.publicProfile) });
    }
    if (url.pathname === '/api/resign' && req.method === 'POST') {
      let request;
      try { request = resignRequest(native.body || await readBody(req)); }
      catch (error) { return json(res, error.status || 400, { error: error.message }); }
      return json(res, 200, { jobs: enqueueResign(request).map(publicBuild) });
    }
    const artifactRoute = url.pathname.match(/^\/api\/artifacts\/([A-Za-z0-9-]{1,64})\/(manifest|download)$/);
    if (url.pathname === '/api/artifacts' && req.method === 'GET') {
      return json(res, 200, { builds: listBuilds({ store: buildStore() }) });
    }
    if (artifactRoute && req.method === 'GET') {
      let kept;
      try { kept = readBuild(artifactRoute[1], { store: buildStore() }); }
      catch { return json(res, 404, { error: 'Build artifact not found or invalid.' }); }
      if (artifactRoute[2] === 'manifest') return json(res, 200, kept.manifest);
      res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': kept.manifest.artifact.bytes,
        'Content-Disposition': `attachment; filename="${kept.manifest.artifact.file}"`, 'Cache-Control': 'no-store',
        'X-Content-Type-Options': 'nosniff' });
      fs.createReadStream(kept.file).on('error', () => res.destroy()).pipe(res);
      return;
    }
    if (url.pathname === '/api/build' && req.method === 'POST') {
      const body = native.body || await readBody(req);
      const jobs = enqueueBuild(body);
      return json(res, 200, { jobs: jobs.map(publicBuild) });
    }
    let m = url.pathname.match(/^\/api\/builds\/(\d+)\/log$/);
    if (m && req.method === 'GET') {
      const id = Number(m[1]);
      const offset = Number(url.searchParams.get('offset') || 0);
      let full = liveLogs.get(id);
      if (full === undefined) {
        try { full = fs.readFileSync(logPath(id), 'utf8'); } catch { full = ''; }
      }
      const b = state.builds.find(x => x.id === id);
      return json(res, 200, {
        chunk: full.slice(offset), nextOffset: full.length,
        status: b ? b.status : 'unknown',
      });
    }
    m = url.pathname.match(/^\/api\/builds\/(\d+)\/cancel$/);
    if (m && req.method === 'POST') {
      return json(res, 200, { ok: cancelBuild(Number(m[1])) });
    }
    if (url.pathname === '/api/refresh-devices' && req.method === 'POST') {
      refreshAllDevices(); // async
      return json(res, 200, { ok: true });
    }
    if (url.pathname === '/api/crashlogs' && req.method === 'GET') {
      return json(res, 200, { crashLogs: crashLogs.list(), pull: crashLogs.status() });
    }
    if (url.pathname === '/api/crashlogs/refresh' && req.method === 'POST') {
      pullCrashLogs(true); // async
      return json(res, 200, { ok: true });
    }
    m = url.pathname.match(/^\/api\/crashlogs\/([0-9a-f-]{8,64})\/(ips|symbolicated)$/);
    if (m && req.method === 'GET') {
      const entry = crashLogs.get(m[1]);
      const rel = entry && (m[2] === 'ips' ? entry.file : entry.symbolicated);
      if (!rel || !fs.existsSync(crashLogs.resolve(rel))) return json(res, 404, { error: 'unknown crash log' });
      // As text, never HTML: the content came from a device.
      res.writeHead(200, { 'Content-Type': 'text/plain; charset=utf-8', 'X-Content-Type-Options': 'nosniff',
        'Content-Security-Policy': "default-src 'none'" });
      fs.createReadStream(crashLogs.resolve(rel)).pipe(res);
      return;
    }
    if (url.pathname === '/api/traces' && req.method === 'POST') {
      try {
        const { status, meta } = await receiveTrace(req);
        return json(res, status, { ok: true, trace: meta });
      } catch (e) {
        if (e instanceof TraceError) return json(res, e.status, { error: e.message });
        throw e;
      }
    }
    if (url.pathname === '/api/traces' && req.method === 'GET') {
      return json(res, 200, { traces: listTraces() });
    }
    m = url.pathname.match(/^\/api\/traces\/([0-9a-f-]{36})\.zip$/);
    if (m && req.method === 'GET') {
      const fp = path.join(TRACE_DIR, m[1], 'trace.zip');
      if (!fs.existsSync(fp)) return json(res, 404, { error: 'unknown trace' });
      const meta = JSON.parse(fs.readFileSync(traceMetaPath(m[1]), 'utf8'));
      const name = safeSlug(`${meta.app?.name || 'app'}-trace-${m[1].slice(0, 8)}.zip`);
      res.writeHead(200, { 'Content-Type': 'application/zip', 'Content-Length': fs.statSync(fp).size,
        'Content-Disposition': `attachment; filename="${name}"` });
      fs.createReadStream(fp).pipe(res);
      return;
    }
    m = url.pathname.match(/^\/api\/traces\/([0-9a-f-]{36})\/files\/(.+)$/);
    if (m && req.method === 'GET') {
      const rel = decodeURIComponent(m[2]);
      const base = path.join(TRACE_DIR, m[1], 'files');
      const fp = path.join(base, rel);
      if (!safeTracePath(rel) || !fp.startsWith(base + path.sep) || !fs.existsSync(fp)) {
        return json(res, 404, { error: 'unknown file' });
      }
      // Text as text so the browser shows it; anything else downloads. Never
      // rendered as HTML: the content came from a device.
      const text = TRACE_TEXT.has(path.extname(fp).toLowerCase());
      res.writeHead(200, {
        'Content-Type': text ? 'text/plain; charset=utf-8' : 'application/octet-stream',
        'X-Content-Type-Options': 'nosniff', 'Content-Security-Policy': "default-src 'none'",
        ...(text ? {} : { 'Content-Disposition': `attachment; filename="${safeSlug(path.basename(fp))}"` }),
      });
      fs.createReadStream(fp).pipe(res);
      return;
    }
    m = url.pathname.match(/^\/api\/traces\/([0-9a-f-]{36})\/delete$/);
    if (m && req.method === 'POST') {
      const dir = path.join(TRACE_DIR, m[1]);
      if (!fs.existsSync(dir)) return json(res, 404, { error: 'unknown trace' });
      fs.rmSync(dir, { recursive: true, force: true });
      return json(res, 200, { ok: true });
    }
    if (url.pathname === '/ota' && req.method === 'GET') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' });
      return res.end(otaPage(req));
    }
    m = url.pathname.match(/^\/ota\/([A-Za-z0-9._-]+)\/manifest\.plist$/);
    if (m && req.method === 'GET') {
      const entry = otaEntryBySlug(m[1]);
      if (!entry) return json(res, 404, { error: 'unknown ipa' });
      const info = { bundleId: entry.bundleId, version: entry.version || '1.0', name: entry.name };
      const plist = otaManifest(signedOtaUrl(req, `/ota/${entry.slug}.ipa`), info);
      res.writeHead(200, { 'Content-Type': 'text/xml; charset=utf-8', 'Cache-Control': 'no-store' });
      return res.end(plist);
    }
    m = url.pathname.match(/^\/ota\/([A-Za-z0-9._-]+)\.ipa$/);
    if (m && req.method === 'GET') {
      const entry = otaEntryBySlug(m[1]);
      const fp = entry && path.join(OTA_DIR, `${entry.slug}.ipa`);
      if (!entry || !fs.existsSync(fp)) return json(res, 404, { error: 'unknown ipa' });
      res.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': fs.statSync(fp).size });
      fs.createReadStream(fp).pipe(res);
      return;
    }
    // static
    if (req.method === 'GET' && sendStatic(res, url.pathname === '/' ? 'index.html' : url.pathname.slice(1))) return;
    json(res, 404, { error: 'not found' });
  } catch (e) {
    json(res, 500, { error: String(e.message || e) });
  }
});

if (require.main === module) {
  watchConfig();
  server.listen(CONFIG.port, '127.0.0.1', () => {
    console.log(`appstore listening on http://127.0.0.1:${CONFIG.port}`);
    refreshAllDevices(); // warm the inventory on startup
    // Crash logs: first look shortly after start, then on a timer.
    setTimeout(() => pullCrashLogs(), 15000);
    setInterval(() => pullCrashLogs(), (CONFIG.crashLogs?.intervalSeconds || 300) * 1000).unref();
    reapBuildClones();
    backfillOta().catch(e => console.error('OTA backfill failed:', e)); // adopt pre-ledger IPAs
  });
}

// For test/: the pure helpers, with the server left unstarted.
module.exports = {
  deployDefaults, staleClones, normalizeExcludes, cloneTree, siblingPackages, verifyTrace, zipEntries, safeTracePath, resolveBuildSettings, TraceError, normalizePrefixes };
