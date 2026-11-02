'use strict';
// macOS apps, built on demand and downloaded as a zipped, signed .app.
//
// config.json, per repo: `macApps: [{name, platform, bundleId, branch}]`. `platform`
// (default "macOS") reaches build-and-sign as PLATFORM, so a repo with several Mac apps
// names each one with an alias its build-signing.conf maps to a scheme and bundle id
// (`macOS-companion` → PLATFORM=macOS, SCHEME_NAME=LongwaveCompanion), the same way an
// `iPad` row aliases the iOS build. `branch` is the one offered by default; a download
// may name any branch the repo offers.
//
// A download names a commit (`/mac/<slug>/<commit>.app.zip`). If that commit is built,
// the zip is served; otherwise the store builds exactly that commit in a clean build
// clone (so the CFBundleVersion stamp is never `-dirty` and the cache can trust the
// SHA), zips the signed .app with ditto and keeps the newest KEEP_PER_APP per app.
// Requests for a commit being built join that build. The request/join/failure logic
// mirrors lib/altsource.js; everything touching the build queue is injected so the
// tests drive it without Xcode.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFile } = require('node:child_process');
const { slugOf, sameCommit, sourceBranch } = require('./altsource');

const KEEP_PER_APP = 3;
const FAILURE_MEMORY_MS = 2 * 60e3;

function run(cmd, args, opts = {}) {
  return new Promise(resolve => execFile(cmd, args, { maxBuffer: 32 << 20, timeout: 600000, ...opts },
    (err, stdout, stderr) => resolve({ code: err ? (err.code ?? 1) : 0, stdout: stdout || '', stderr: stderr || '' })));
}

// Every configured Mac app, keyed `<bundleId>-<platform>` like the OTA and
// AltSource slugs. Entries without a bundleId are skipped. Pure, for test/.
function macApps(config) {
  const out = [];
  for (const rc of config.repos || []) {
    for (const m of Array.isArray(rc.macApps) ? rc.macApps : []) {
      if (!m || typeof m.bundleId !== 'string' || !m.bundleId) continue;
      const platform = typeof m.platform === 'string' && m.platform ? m.platform : 'macOS';
      const slug = slugOf(`${m.bundleId}-${platform}`);
      if (out.some(a => a.slug === slug)) continue;
      out.push({
        slug, repo: rc.name, path: rc.path, name: m.name || rc.name, platform, bundleId: m.bundleId,
        branch: typeof m.branch === 'string' ? m.branch : sourceBranch(rc),
      });
    }
  }
  return out;
}

// Name, bundle id, version and stamp of a built .app, from Contents/Info.plist.
async function inspectApp(appPath) {
  const r = await run('plutil', ['-convert', 'json', '-o', '-', '--', path.join(appPath, 'Contents', 'Info.plist')]);
  if (r.code !== 0) throw new Error(`could not read the Info.plist of ${path.basename(appPath)}`);
  const info = JSON.parse(r.stdout);
  return {
    name: info.CFBundleDisplayName || info.CFBundleName || path.basename(appPath, '.app'),
    bundleId: info.CFBundleIdentifier,
    version: info.CFBundleShortVersionString || '1.0',
    buildNo: info.CFBundleVersion || '',
    minOSVersion: info.LSMinimumSystemVersion || null,
  };
}

// build-and-sign leaves the signed app as build/<Target>-macOS.app; it goes into
// the zip under its product name. Renaming the bundle directory leaves the
// signature intact. ditto keeps the framework symlinks and extended attributes
// that a plain zip would break the signature over.
async function zipApp(appPath, dest) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'store-mac-'));
  try {
    const named = path.join(tmp, path.basename(appPath).replace(/-macOS(?=\.app$)/, ''));
    const copy = await run('cp', ['-cR', appPath, named]);
    if (copy.code !== 0) throw new Error(`could not stage ${path.basename(appPath)}: ${copy.stderr.trim()}`);
    const z = await run('ditto', ['-c', '-k', '--sequesterRsrc', '--keepParent', named, dest]);
    if (z.code !== 0) throw new Error(`could not zip ${path.basename(named)}: ${z.stderr.trim()}`);
    return path.basename(named);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// dir: where the zips and index.json live. apps(): macApps of the live config.
// enqueue(app, commit, branch) -> job {id}. job(id) -> job | undefined.
// waitFor(id) -> Promise settling when the build ends. inspect/pack: injected for tests.
function createMacBuilds({ dir, apps, enqueue, job, waitFor, inspect = inspectApp, pack = zipApp, now = Date.now, log = () => {} }) {
  fs.mkdirSync(dir, { recursive: true });
  const indexPath = path.join(dir, 'index.json');
  let index = { apps: {} };
  try { index = { ...index, ...JSON.parse(fs.readFileSync(indexPath, 'utf8')) }; } catch { /* first run */ }
  const inFlight = new Map(); // `${slug}@${commit}` -> build id
  const failed = new Map();   // `${slug}@${commit}` -> {error, at}

  function save() {
    const tmp = indexPath + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(index, null, 1));
    fs.renameSync(tmp, indexPath);
  }
  const entries = slug => (index.apps[slug] || []).filter(e => fs.existsSync(path.join(dir, e.file)));
  const cached = (slug, commit) => entries(slug).find(e => sameCommit(e.commit, commit)) || null;
  const appBySlug = slug => apps().find(a => a.slug === slug) || null;

  // Zip a signed .app as this app's build of `commit`, newest first. Refuses an
  // app that isn't the one configured: a conf alias that fell through to another
  // scheme would otherwise be cached and served under this app's name.
  async function record(slug, commit, branch, appPath, kept = null) {
    const app = appBySlug(slug);
    if (!app) throw new Error(`unknown Mac app ${slug}`);
    if (kept && kept.manifest.artifact.kind !== 'app.zip') throw new Error('Mac build manifest does not describe an app zip');
    const meta = kept ? { bundleId: kept.manifest.app.bundleId, buildNo: kept.manifest.app.build,
      version: kept.manifest.app.version, name: kept.manifest.app.name, minOSVersion: kept.manifest.app.minOSVersion } : await inspect(appPath);
    if (meta.bundleId !== app.bundleId) {
      throw new Error(`the build produced ${meta.bundleId || 'an app with no bundle id'}, not ${app.bundleId}: check the ${app.platform} block of ${app.repo}'s build-signing.conf`);
    }
    const file = `${slug}-${slugOf(commit)}.app.zip`;
    const tmp = path.join(dir, `.incoming-${file}`);
    fs.rmSync(tmp, { force: true });
    let bundle;
    if (kept) {
      fs.copyFileSync(kept.file, tmp, fs.constants.COPYFILE_FICLONE);
      bundle = path.basename(appPath);
    } else bundle = await pack(appPath, tmp);
    fs.renameSync(tmp, path.join(dir, file));
    const entry = {
      commit, branch, buildNo: meta.buildNo, version: meta.version, name: meta.name, bundle,
      minOSVersion: meta.minOSVersion, file, size: fs.statSync(path.join(dir, file)).size, at: now(),
      buildId: kept?.manifest.buildId || null, artifactSha256: kept?.manifest.artifact.sha256 || null,
    };
    const list = [entry, ...(index.apps[slug] || []).filter(e => e.file !== file)];
    for (const old of list.slice(KEEP_PER_APP)) fs.rmSync(path.join(dir, old.file), { force: true });
    index.apps[slug] = list.slice(0, KEEP_PER_APP);
    save();
    if (meta.buildNo && !sameCommit(meta.buildNo.replace(/-dirty$/, ''), commit)) {
      log(`macapps: ${slug} built for ${commit} is stamped ${meta.buildNo}`);
    }
    return entry;
  }

  // What a request for `slug`@`commit` gets right now:
  //   {entry}          — built; serve entry.file
  //   {jobId}          — a build is queued or running; wait on it
  //   {status, error}  — unknown app, or its build just failed
  function request(slug, commit, branch) {
    const app = appBySlug(slug);
    if (!app) return { status: 404, error: 'unknown Mac app' };
    const hit = cached(slug, commit);
    if (hit) return { entry: hit };
    const key = `${slug}@${commit}`;
    const running = inFlight.get(key);
    if (running != null) {
      const j = job(running);
      if (j && (j.status === 'queued' || j.status === 'running')) return { jobId: running };
      inFlight.delete(key);
    }
    const f = failed.get(key);
    if (f && now() - f.at < FAILURE_MEMORY_MS) return { status: 502, error: f.error };
    const j = enqueue(app, commit, branch || app.branch);
    inFlight.set(key, j.id);
    failed.delete(key);
    waitFor(j.id).then(() => {
      inFlight.delete(key);
      const done = job(j.id);
      if (done && done.status !== 'success') failed.set(key, { error: `build ${done.status}: ${done.error || 'no app'}`, at: now() });
    });
    return { jobId: j.id };
  }

  // After waiting on a build: the zip, or why there isn't one.
  function outcome(slug, commit, jobId) {
    const hit = cached(slug, commit);
    if (hit) return { entry: hit };
    const j = job(jobId);
    if (j && (j.status === 'queued' || j.status === 'running')) return { jobId };
    return { status: 502, error: j ? `build ${j.status}: ${j.error || 'it produced no app'}` : 'build lost' };
  }

  // A repo's Mac apps for the page, each with its kept builds (newest first)
  // and the build of it in progress, if any.
  function list(repo) {
    return apps().filter(a => a.repo === repo).map(a => ({
      slug: a.slug, name: a.name, platform: a.platform, bundleId: a.bundleId, branch: a.branch,
      built: entries(a.slug).map(({ file, ...e }) => e),
      building: [...inFlight.entries()].filter(([k]) => k.startsWith(`${a.slug}@`)).map(([k, id]) => ({ commit: k.slice(a.slug.length + 1), jobId: id })),
    }));
  }

  return { record, request, outcome, cached, list, appBySlug, file: e => path.join(dir, e.file) };
}

module.exports = { createMacBuilds, macApps, inspectApp, zipApp };
