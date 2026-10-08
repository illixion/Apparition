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

const ROOT = __dirname;
// APPSTORE_CONFIG and APPSTORE_DATA relocate the config and the runtime data
// (the tests use them); by default both live next to this file.
const CONFIG_PATH = process.env.APPSTORE_CONFIG || path.join(ROOT, 'config.json');
if (!fs.existsSync(CONFIG_PATH)) {
  console.error('config.json not found. Copy config.example.json to config.json and edit it.');
  process.exit(1);
}
let CONFIG = JSON.parse(fs.readFileSync(CONFIG_PATH, 'utf8'));
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
const watchConfig = () => fs.watch(CONFIG_PATH, (eventType, filename) => {
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
function clonePath(repoPath, branch) {
  return path.join(CLONE_DIR, safeSlug(path.basename(repoPath)), safeSlug(branch));
}
async function ensureBuildClone(repoPath, branch) {
  const dst = clonePath(repoPath, branch);
  fs.rmSync(dst, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(dst), { recursive: true });
  const clone = await run('cp', ['-cR', repoPath, dst], { timeout: 300000 });
  if (clone.code !== 0) throw new Error(`cp -cR clone of ${repoPath} failed: ${clone.stderr.trim()}`);
  // bring the clone's target branch up to the main repo's latest local
  // commit — fetch from the main repo (a plain local-path remote) since
  // cp -cR only copied whatever ref state existed at clone time
  const fetch = await run('git', ['-C', dst, 'fetch', repoPath, `+refs/heads/${branch}:refs/heads/${branch}`]);
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
  const reset = await run('git', ['-C', dst, 'reset', '--hard', branch]);
  if (reset.code !== 0) throw new Error(`git reset ${branch} in clone failed: ${reset.stderr.trim()}`);
  const clean = await run('git', ['-C', dst, 'clean', '-fd']);
  if (clean.code !== 0) throw new Error(`git clean in clone failed: ${clean.stderr.trim()}`);
  return dst;
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
  })).filter(d => d.name);
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
const procs = new Map();    // buildId -> ChildProcess (NEVER stored on the job: jobs are JSON-serialized)

function pump(repo) {
  const q = getRepoQueue(repo);
  if (q.activeId != null) return;
  const id = q.queue.shift();
  if (id == null) return;
  const job = state.builds.find(b => b.id === id);
  if (!job || job.status === 'cancelled') { pump(repo); return; }
  q.activeId = id;
  job.status = 'running';
  job.startedAt = Date.now();
  saveState();
  executeBuild(job).finally(() => { q.activeId = null; pump(repo); });
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
// launch = --run; buildDebug = --debug (Debug configuration); inject = --inject
// (patchable build for live code injection)
function enqueueBuild({ repo, targets, device, branch, run: launch, buildDebug, inject, deploy }) {
  const rc = CONFIG.repos.find(r => r.name === repo);
  if (!rc) throw new Error(`unknown repo: ${repo}`);
  const targetList = (targets && targets.length) ? targets : [{ device: device || rc.platforms[0].device, branch }];
  const jobs = targetList.map(t => {
    const pc = rc.platforms.find(p => p.device === t.device) || rc.platforms[0];
    return {
      id: state.nextBuildId++,
      batchId: null,
      repo, branch: t.branch || 'current',
      platform: pc.platform, device: pc.device, bundleId: pc.bundleId,
      // deploy:false = OTA-only build — build-and-sign gets --no-deploy so an
      // unreachable device can't fail the build; the run/launch step and the
      // install ledger are skipped too (nothing landed on the device)
      noDeploy: deploy === false,
      run: !!launch && deploy !== false,
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

  try {
    const orig = (await run('git', ['-C', repoPath, 'rev-parse', '--abbrev-ref', 'HEAD'])).stdout.trim();
    let buildCwd = repoPath;
    if (job.branch !== 'current' && job.branch !== orig) {
      log(`==> Using dedicated build clone for ${job.branch} (main checkout stays on ${orig})\n`);
      buildCwd = await ensureBuildClone(repoPath, job.branch);
    }
    const head = (await run('git', ['-C', buildCwd, 'log', '-1', '--format=%h %s'])).stdout.trim();
    job.commit = head.split(' ')[0];
    job.builtBranch = job.branch === 'current' ? orig : job.branch;
    job.buildSettings = resolveBuildSettings(rc, job.builtBranch, job.platform);
    log(`==> Building ${job.repo} [${job.builtBranch}@${job.commit}] platform=${job.platform} → ${job.device}\n`);
    if (job.buildSettings.length) log(`    build settings: ${job.buildSettings.join('  ')}\n`);
    log('\n');
    gitCache.delete(buildCwd);

    const buildArgs = [CONFIG.buildAndSign || DEFAULT_BUILD_AND_SIGN];
    if (job.noDeploy) buildArgs.push('--no-deploy');
    // Always say it: build-and-sign may default to --mcp (MCP_DEFAULT=on), which
    // would turn a plain install or launch into a debug session. The Debug tab
    // connects to the app's debug server on any launch, so the store never asks.
    if (!job.noDeploy) buildArgs.push('--no-mcp');
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
        env: { ...process.env, PLATFORM: job.platform },
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

    // read the version actually installed from the SIGNED IPA (the staging
    // build/Payload copy predates the git-hash CFBundleVersion stamp)
    let version = null, buildNo = null, ipaPath = null, appName = null;
    try {
      const buildDir = path.join(buildCwd, 'build');
      const ipa = fs.readdirSync(buildDir).filter(f => f.endsWith('-signed.ipa'))
        .map(f => ({ f, m: fs.statSync(path.join(buildDir, f)).mtimeMs }))
        .sort((a, b) => b.m - a.m)[0].f;
      ipaPath = path.join(buildDir, ipa);
      const info = await readIpaInfo(ipaPath, fs.statSync(ipaPath).mtimeMs);
      version = info.version;
      buildNo = info.buildNo;
      appName = info.name;
    } catch { /* non-fatal */ }
    job.version = version;
    job.buildNo = buildNo;

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
    state.installs[job.device] = state.installs[job.device] || {};
    state.installs[job.device][job.bundleId] = {
      repo: job.repo, branch: job.builtBranch, commit: job.commit,
      platform: job.platform, version, buildNo, at: Date.now(),
      confirmed: !job.noDeploy,
    };
    job.status = 'success';
    log(job.noDeploy
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
    job.finishedAt = Date.now();
    saveState();
    stream.end();
    // keep only the last few live logs in memory; files persist
    if (liveLogs.size > 5) liveLogs.delete(liveLogs.keys().next().value);
  }
  if (job.status === 'success' && !job.noDeploy) refreshDeviceApps(job.device); // async, best-effort
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
      for (const b of fs.readdirSync(cloneRoot)) dirs.push(path.join(cloneRoot, b, 'build'));
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
  for (const r of CONFIG.repos) if ((r.platforms || []).some(p => p.bundleId === bundleId)) return r.name;
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
      keyId: key.keyId, repo: repoForBundle(key.bundleId), branch: key.branch || null,
      platform: key.platform || null, configuration: key.configuration || null,
      installedTo: key.device || null, builtAt: key.createdAt || null,
    },
    files: files.map(f => ({ path: f.path, bytes: f.data.length })),
  };
  fs.writeFileSync(path.join(tmp, 'meta.json'), JSON.stringify(meta, null, 1));
  fs.renameSync(tmp, dir);
  pruneTraces();
  console.log(`[${new Date().toISOString()}] trace ${id} from ${manifest.app.bundleId} ${manifest.app.build} (${files.length} files, ${buf.length} bytes)`);
  return { status: 201, meta };
}

const TRACE_TEXT = new Set(['.txt', '.json', '.jsonl', '.md', '.log', '.csv']);

// ---------- HTTP ----------
function json(res, code, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(code, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
  res.end(body);
}

function publicBuild(b) {
  return b;
}

// What the deploy menu selects at first: "install", "run" or "debug" (launch
// as a debug session), from the repo's `afterInstall`, else the top-level one,
// else "install".
const AFTER_INSTALL = ['install', 'run', 'debug'];
function afterInstallDefault(repo) {
  const pick = [repo.afterInstall, CONFIG.afterInstall].find(v => AFTER_INSTALL.includes(v));
  return pick || 'install';
}

async function stateBundle() {
  const repos = await Promise.all(CONFIG.repos.map(async r => ({
    name: r.name, branches: r.branches, platforms: r.platforms,
    deviceBranches: r.deviceBranches || {},
    afterInstall: afterInstallDefault(r),
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
      if (sendStatic(res, url.pathname === '/login' ? 'login.html' : url.pathname.slice(1))) return;
    }
    // Apps upload traces with no session; the signature check is the gate.
    const traceUpload = url.pathname === '/api/traces' && req.method === 'POST';
    // installd can't send the cookie, so OTA files also accept a signed ?t=.
    const otaFile = req.method === 'GET' && /^\/ota\/[A-Za-z0-9._-]+(\/manifest\.plist|\.ipa)$/.test(url.pathname)
      && auth.verifyPath(url.pathname, url.searchParams.get('t'));

    if (!traceUpload && !otaFile && !signedIn(req)) {
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

    if (url.pathname === '/api/state' && req.method === 'GET') {
      return json(res, 200, await stateBundle());
    }
    if (url.pathname === '/api/build' && req.method === 'POST') {
      const body = await readBody(req);
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
    backfillOta().catch(e => console.error('OTA backfill failed:', e)); // adopt pre-ledger IPAs
  });
}

// For test/: the pure helpers, with the server left unstarted.
module.exports = {
  afterInstallDefault, verifyTrace, zipEntries, safeTracePath, resolveBuildSettings, TraceError, normalizePrefixes };
