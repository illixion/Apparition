'use strict';
// An AltStore-format source of the store's apps, built on demand.
//
// `GET /altsource/source.json?k=<feed token>` lists every configured app at the head of its
// source branch: `buildVersion` is that commit's short SHA, which is what build-and-sign
// stamps into CFBundleVersion, so a client (AltStore, SideStore, Hermes) sees an update
// whenever the branch moves. Each version's downloadURL names the commit
// (`/altsource/<slug>/<commit>.ipa`). Requesting it serves the IPA if that commit is
// already built; otherwise it starts a build of exactly that commit (from a clean build
// clone, so the stamp carries no `-dirty`) and holds the request open until it finishes.
// The hold is a long poll: a client that times out between packets (URLSession's default
// is 60 s) gets a 307 back to the same URL before then and waits again, while the build
// carries on regardless of who is listening.
//
// AltStore refuses an IPA whose entitlements or privacy strings differ from the source's
// `appPermissions`, and whose version or build differ from the listing, so both are read
// from the built IPA. Before a commit is built, the listing carries the previous build's
// values, which only change when the app's capabilities do.
//
// Everything that touches the build queue is injected (`enqueue`, `job`, `waitFor`), so the
// tests drive this without Xcode.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFile } = require('node:child_process');

const KEEP_PER_APP = 2;
// A failed build is reported to further requests for this long instead of starting
// another, so a client that retries on its own doesn't queue a build each time.
const FAILURE_MEMORY_MS = 2 * 60e3;
// Entitlements AltStore says a source need not list.
const IMPLICIT_ENTITLEMENTS = new Set(['application-identifier', 'com.apple.developer.team-identifier']);

const slugOf = s => String(s).replace(/[^A-Za-z0-9._-]+/g, '-');
// Short SHAs from two places may be abbreviated to different lengths.
const sameCommit = (a, b) => !!a && !!b && (a.startsWith(b) || b.startsWith(a));

function run(cmd, args, opts = {}) {
  return new Promise(resolve => execFile(cmd, args, { maxBuffer: 32 << 20, timeout: 300000, ...opts },
    (err, stdout, stderr) => resolve({ code: err ? (err.code ?? 1) : 0, stdout: stdout || '', stderr: stderr || '' })));
}

// The branch a repo's source entry follows: its own `altSource.branch`, the
// source-wide one, else main/master when offered, else the first branch offered.
function sourceBranch(rc, cfg = {}) {
  if (typeof rc.altSource?.branch === 'string') return rc.altSource.branch;
  if (typeof cfg.branch === 'string' && (rc.branches || []).includes(cfg.branch)) return cfg.branch;
  const b = rc.branches || [];
  return b.includes('main') ? 'main' : b.includes('master') ? 'master' : (b[0] || 'main');
}

// The platform rows a repo offers to the feed: all of them, or those its
// `altSource.platforms` names; none with `altSource: false`.
function feedRows(rc) {
  if (rc.altSource === false) return [];
  const own = rc.altSource && typeof rc.altSource === 'object' ? rc.altSource : {};
  return Array.isArray(own.platforms) ? (rc.platforms || []).filter(p => own.platforms.includes(p.platform)) : (rc.platforms || []);
}

// Every platform name some repo offers, in config order: one source each.
// These are config.json's names, aliases included (`iPad`, a visionOS row that
// builds the iPad binary), since that is the device the list is for. Pure, for test/.
function sourcePlatforms(config) {
  const out = [];
  for (const rc of config.repos || []) for (const pc of feedRows(rc)) if (!out.includes(pc.platform)) out.push(pc.platform);
  return out;
}

// The apps of one platform's source (`platform` set) or of the combined one.
// A source can't list a bundle id twice, so the first row naming it wins: in a
// platform source that only matters for two rows of one platform (two iPhones).
// Each app is keyed by bundle id and platform, since one bundle id can build
// differently per platform (an alias with its own DEVICE_NAME). Pure, for test/.
function sourceApps(config, platform = null) {
  const cfg = config.altSource || {};
  const seen = new Set();
  const out = [];
  for (const rc of config.repos || []) {
    const own = rc.altSource && typeof rc.altSource === 'object' ? rc.altSource : {};
    for (const pc of feedRows(rc)) {
      if (platform && pc.platform !== platform) continue;
      if (seen.has(pc.bundleId)) continue;
      seen.add(pc.bundleId);
      out.push({
        slug: slugOf(`${pc.bundleId}-${pc.platform}`), repo: rc.name, path: rc.path, bundleId: pc.bundleId,
        platform: pc.platform, device: pc.device, branch: sourceBranch(rc, cfg),
        description: own.description, subtitle: own.subtitle, iconURL: own.iconURL,
        developerName: own.developerName || cfg.developerName, category: own.category,
      });
    }
  }
  return out;
}

// Every app any source lists, once per slug: what a download URL can name.
function allSourceApps(config) {
  const out = sourceApps(config);
  for (const p of sourcePlatforms(config)) for (const a of sourceApps(config, p)) if (!out.some(b => b.slug === a.slug)) out.push(a);
  return out;
}

// Entitlement keys and privacy strings of every bundle in an extracted Payload,
// plus the largest top-level AppIcon PNG (iOS bundles carry them beside
// Assets.car; visionOS ones only have the catalog).
async function inspectPayload(root) {
  const payload = path.join(root, 'Payload');
  const app = fs.readdirSync(payload).find(n => n.endsWith('.app'));
  if (!app) throw new Error('no .app in Payload');
  const appDir = path.join(payload, app);
  const plist = async file => {
    const r = await run('plutil', ['-convert', 'json', '-o', '-', '--', file]);
    if (r.code !== 0) throw new Error(`could not read ${path.basename(file)}`);
    return JSON.parse(r.stdout);
  };
  const info = await plist(path.join(appDir, 'Info.plist'));
  const bundles = [];
  const walk = (dir, depth) => {
    if (depth > 4) return;
    for (const n of fs.readdirSync(dir)) {
      const p = path.join(dir, n);
      let st; try { st = fs.lstatSync(p); } catch { continue; }
      if (!st.isDirectory()) continue;
      if (/\.(app|appex)$/.test(n)) bundles.push(p);
      if (!/\.(lproj|car|bundle|framework|dSYM)$/.test(n)) walk(p, depth + 1);
    }
  };
  bundles.push(appDir);
  walk(appDir, 0);
  const entitlements = new Set();
  const privacy = {};
  for (const b of bundles) {
    const out = path.join(root, `ent-${bundles.indexOf(b)}.plist`);
    const r = await run('codesign', ['-d', '--entitlements', out, '--xml', b]);
    if (r.code === 0 && fs.existsSync(out) && fs.statSync(out).size > 0) {
      try { for (const k of Object.keys(await plist(out))) if (!IMPLICIT_ENTITLEMENTS.has(k)) entitlements.add(k); } catch { /* unsigned or empty */ }
    }
    let bi = info;
    if (b !== appDir) { try { bi = await plist(path.join(b, 'Info.plist')); } catch { continue; } }
    for (const [k, v] of Object.entries(bi)) {
      if (/UsageDescription$/.test(k) && typeof v === 'string' && !(k in privacy)) privacy[k] = v;
    }
  }
  const icon = fs.readdirSync(appDir)
    .map(n => { const m = /^AppIcon(\d+(?:\.\d+)?)x\d+(?:\.\d+)?(?:@(\d)x)?(?:~ipad)?\.png$/.exec(n); return m && { n, px: Number(m[1]) * Number(m[2] || 1) }; })
    .filter(Boolean).sort((a, b) => b.px - a.px)[0];
  return {
    name: info.CFBundleDisplayName || info.CFBundleName || app.replace(/\.app$/, ''),
    bundleId: info.CFBundleIdentifier,
    version: info.CFBundleShortVersionString || '1.0',
    buildNo: info.CFBundleVersion || '',
    minOSVersion: info.MinimumOSVersion || null,
    permissions: { entitlements: [...entitlements].sort(), privacy },
    iconFile: icon ? path.join(appDir, icon.n) : null,
  };
}

async function inspectIpa(ipaPath) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-altsource-'));
  try {
    const x = await run('ditto', ['-x', '-k', ipaPath, tmp]);
    if (x.code !== 0) throw new Error(`could not unpack ${path.basename(ipaPath)}: ${x.stderr.trim()}`);
    const r = await inspectPayload(tmp);
    if (r.iconFile) r.icon = fs.readFileSync(r.iconFile);
    delete r.iconFile;
    return r;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// The AltStore source document. `apps` carry what's known per app: `head`
// {commit, date, subject} and `built` (the newest recorded IPA, or null).
// Pure, for test/. The flat `version`/`downloadURL`/... app keys are the
// pre-2.0 layout older SideStore builds still read.
function buildSource({ cfg = {}, title, apps, url, platform = null }) {
  const list = apps.filter(a => a.head).map(a => {
    const b = a.built && sameCommit(a.built.commit, a.head.commit) ? a.built : null;
    const prev = a.built;
    const version = {
      version: (b || prev)?.version || '1.0',
      // The real stamp when this commit is built (it could say -dirty), else the SHA it will get.
      buildVersion: b ? b.buildNo : a.head.commit,
      date: a.head.date,
      localizedDescription: a.head.subject || '',
      downloadURL: url(`/altsource/${a.slug}/${a.head.commit}.ipa`),
      size: (b || prev)?.size || 0,
      ...((b || prev)?.minOSVersion ? { minOSVersion: (b || prev).minOSVersion } : {}),
    };
    return {
      name: (b || prev)?.name || a.repo,
      bundleIdentifier: a.bundleId,
      developerName: a.developerName || cfg.developerName || 'Me',
      subtitle: a.subtitle || `${a.repo} · ${a.branch}`,
      localizedDescription: a.description || `${a.repo}, built from ${a.branch} on demand.`,
      iconURL: a.iconURL || url(`/altsource/${a.slug}/icon.png`),
      category: a.category || 'developer',
      // Not part of the format: lets a client (Hermes) pick the apps for its platform.
      platform: a.platform,
      versions: [version],
      appPermissions: (b || prev)?.permissions || { entitlements: [], privacy: {} },
      screenshots: [],
      version: version.version, versionDate: version.date, versionDescription: version.localizedDescription,
      downloadURL: version.downloadURL, size: version.size,
    };
  });
  return {
    // AltStore keys sources by identifier, so each platform's needs its own.
    name: platform ? `${cfg.name || title} (${platform})` : (cfg.name || title),
    identifier: (cfg.identifier || 'local.appstore.source') + (platform ? `.${slugOf(platform).toLowerCase()}` : ''),
    subtitle: cfg.subtitle || 'Dev builds, built when you ask for them',
    iconURL: cfg.iconURL || url('/altsource/icon.png'),
    website: cfg.website || undefined,
    tintColor: cfg.tintColor || '#4f8cff',
    apps: list,
    news: [],
  };
}

// dir: where IPAs and index.json live. apps(platform|null): sourceApps of the
// live config; allApps(): allSourceApps of it.
// head(app) -> {commit, date, subject}. enqueue(app, commit) -> job {id}.
// job(id) -> job | undefined. waitFor(id) -> Promise settling when the build ends.
// inspect(ipa) -> inspectIpa's shape (injected for tests).
function createAltSource({ dir, apps, allApps, head, enqueue, job, waitFor, inspect = inspectIpa, now = Date.now, log = () => {} }) {
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
  const latest = slug => entries(slug)[0] || null;
  const cached = (slug, commit) => entries(slug).find(e => sameCommit(e.commit, commit)) || null;
  const appBySlug = slug => allApps().find(a => a.slug === slug) || null;

  // Keep a copy of a signed IPA as this app's build of `commit`, newest first,
  // KEEP_PER_APP per app.
  async function record(slug, commit, ipaPath) {
    const meta = await inspect(ipaPath);
    const base = `${slug}-${slugOf(commit)}`;
    const file = `${base}.ipa`;
    fs.rmSync(path.join(dir, file), { force: true });
    fs.copyFileSync(ipaPath, path.join(dir, file), fs.constants.COPYFILE_FICLONE);
    let icon = null;
    if (meta.icon) { icon = `${base}.png`; fs.writeFileSync(path.join(dir, icon), meta.icon); }
    const entry = {
      commit, buildNo: meta.buildNo, version: meta.version, name: meta.name, minOSVersion: meta.minOSVersion,
      permissions: meta.permissions, file, icon, size: fs.statSync(path.join(dir, file)).size, at: now(),
    };
    const list = [entry, ...(index.apps[slug] || []).filter(e => e.file !== file)];
    for (const old of list.slice(KEEP_PER_APP)) {
      for (const f of [old.file, old.icon]) if (f) fs.rmSync(path.join(dir, f), { force: true });
    }
    index.apps[slug] = list.slice(0, KEEP_PER_APP);
    save();
    if (meta.buildNo && !sameCommit(meta.buildNo.replace(/-dirty$/, ''), commit)) {
      log(`altsource: ${slug} built for ${commit} is stamped ${meta.buildNo}`);
    }
    return entry;
  }

  // What a download request for `slug`@`commit` gets right now:
  //   {entry}            — built; serve entry.file
  //   {jobId}            — a build is queued or running; wait on it
  //   {status, error}    — unknown app, or its build just failed
  // `adopt(app, commit)` may return the path of an IPA built elsewhere (a LAN
  // deploy) whose stamp is exactly this commit, so it needn't be built again.
  async function request(slug, commit, { adopt } = {}) {
    const app = appBySlug(slug);
    if (!app) return { status: 404, error: 'unknown app' };
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
    const other = adopt && adopt(app, commit);
    if (other) {
      try { return { entry: await record(slug, commit, other) }; } catch (e) { log(`altsource: could not adopt ${other}: ${e.message}`); }
    }
    const j = enqueue(app, commit);
    inFlight.set(key, j.id);
    failed.delete(key);
    waitFor(j.id).then(() => {
      inFlight.delete(key);
      const done = job(j.id);
      if (done && done.status !== 'success') failed.set(key, { error: `build ${done.status}: ${done.error || 'no IPA'}`, at: now() });
    });
    return { jobId: j.id };
  }

  // After waiting on a build: the IPA, or why there isn't one.
  function outcome(slug, commit, jobId) {
    const hit = cached(slug, commit);
    if (hit) return { entry: hit };
    const j = job(jobId);
    if (j && (j.status === 'queued' || j.status === 'running')) return { jobId };
    return { status: 502, error: j ? `build ${j.status}: ${j.error || 'it produced no IPA'}` : 'build lost' };
  }

  async function source({ title, url, cfg, platform = null }) {
    const list = await Promise.all(apps(platform).map(async a => {
      let h = null;
      try { h = await head(a); } catch (e) { log(`altsource: no head for ${a.repo}@${a.branch}: ${e.message}`); }
      return { ...a, head: h, built: latest(a.slug) };
    }));
    return buildSource({ cfg, title, apps: list, url, platform });
  }

  return {
    record, request, outcome, source, cached, latest, appBySlug,
    file: e => path.join(dir, e.file),
    iconFile: slug => { const e = entries(slug).find(x => x.icon); return e ? path.join(dir, e.icon) : null; },
    building: () => [...inFlight.entries()].map(([k, id]) => ({ key: k, jobId: id })),
  };
}

module.exports = { createAltSource, sourceApps, sourcePlatforms, allSourceApps, sourceBranch, buildSource, inspectIpa, inspectPayload, sameCommit, slugOf };
