'use strict';
// The AltSource feed: the pure listing helpers, the build-join logic with a fake
// queue, and the whole long poll through the real server with a fake build-and-sign.
const { test, describe, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const net = require('node:net');
const http = require('node:http');
const path = require('node:path');
const { spawn, execFileSync } = require('node:child_process');
const { hashPassword } = require('../lib/auth');
const { createAltSource, sourceApps, sourcePlatforms, allSourceApps, sourceBranch, buildSource, sameCommit } = require('../lib/altsource');

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-altsource-'));

describe('listing', () => {
  const config = {
    altSource: { enabled: true, developerName: 'Ixion' },
    repos: [
      { name: 'A', path: '/a', branches: ['dev', 'main'], platforms: [
        { platform: 'iOS', bundleId: 'com.example.a', device: 'Phone' },
        { platform: 'iPad', bundleId: 'com.example.a', device: 'Pad' },
        { platform: 'visionOS', bundleId: 'com.example.a.vision', device: 'AVP' },
        { platform: 'watchOS', bundleId: 'com.example.a.watchkitapp', device: 'Watch' }] },
      { name: 'B', path: '/b', branches: ['feature'], altSource: false, platforms: [
        { platform: 'iOS', bundleId: 'com.example.b', device: 'Phone' }] },
      { name: 'C', path: '/c', branches: ['x', 'y'], altSource: { branch: 'y', platforms: ['tvOS'] }, platforms: [
        { platform: 'iOS', bundleId: 'com.example.c', device: 'Phone' },
        { platform: 'tvOS', bundleId: 'com.example.c.tv', device: 'ATV' }] },
    ],
  };

  test('one app per bundle id, opted-out repos left out, platforms filterable', () => {
    const apps = sourceApps(config);
    assert.deepEqual(apps.map(a => [a.bundleId, a.platform, a.branch]), [
      ['com.example.a', 'iOS', 'main'],
      ['com.example.a.vision', 'visionOS', 'main'],
      ['com.example.c.tv', 'tvOS', 'y'],
    ]);
    assert.equal(apps[0].developerName, 'Ixion');
    assert.equal(apps[0].slug, 'com.example.a-iOS');
  });

  test('a watchOS row is never a feed app: the watch app ships inside the iPhone IPA', () => {
    assert.ok(!allSourceApps(config).some(a => a.platform === 'watchOS'));
  });

  test('one source per offered platform name, aliases included', () => {
    assert.deepEqual(sourcePlatforms(config), ['iOS', 'iPad', 'visionOS', 'tvOS']);
  });

  test('a platform source lists that platform\'s rows, keyed per platform', () => {
    assert.deepEqual(sourceApps(config, 'iPad').map(a => [a.bundleId, a.device, a.slug]), [['com.example.a', 'Pad', 'com.example.a-iPad']]);
    assert.deepEqual(sourceApps(config, 'visionOS').map(a => a.bundleId), ['com.example.a.vision']);
    assert.deepEqual(sourceApps(config, 'tvOS').map(a => a.bundleId), ['com.example.c.tv']);
    // C's iOS row is filtered out by its altSource.platforms, B is opted out
    assert.deepEqual(sourceApps(config, 'iOS').map(a => a.bundleId), ['com.example.a']);
    assert.deepEqual(sourceApps(config, 'watchOS'), []);
  });

  test('every slug any source lists can be downloaded', () => {
    assert.deepEqual(allSourceApps(config).map(a => a.slug).sort(),
      ['com.example.a-iOS', 'com.example.a-iPad', 'com.example.a.vision-visionOS', 'com.example.c.tv-tvOS']);
  });

  test('platform sources get their own name and identifier', () => {
    const doc = buildSource({ cfg: { name: 'N', identifier: 'x.y' }, title: 'T', apps: [], url: p => p, platform: 'visionOS' });
    assert.equal(doc.name, 'N (visionOS)');
    assert.equal(doc.identifier, 'x.y.visionos');
    assert.equal(buildSource({ cfg: { identifier: 'x.y' }, title: 'T', apps: [], url: p => p }).identifier, 'x.y');
  });

  test('branch: own, then source-wide if offered, then main/master, then first', () => {
    assert.equal(sourceBranch({ branches: ['a', 'b'], altSource: { branch: 'b' } }), 'b');
    assert.equal(sourceBranch({ branches: ['a', 'b'] }, { branch: 'b' }), 'b');
    assert.equal(sourceBranch({ branches: ['a', 'master'] }, { branch: 'zzz' }), 'master');
    assert.equal(sourceBranch({ branches: ['a', 'b'] }), 'a');
  });

  test('short SHAs of different lengths match', () => {
    assert.ok(sameCommit('abc1234', 'abc12345'));
    assert.ok(!sameCommit('abc1234', 'abd1234'));
    assert.ok(!sameCommit('', 'abc'));
  });

  const url = p => `https://h${p}?t=x`;
  const app = { slug: 'com.example.a', repo: 'A', bundleId: 'com.example.a', platform: 'iOS', branch: 'main' };
  const perms = { entitlements: ['get-task-allow'], privacy: { NSCameraUsageDescription: 'cam' } };

  test('a built head lists its real stamp, permissions and size', () => {
    const doc = buildSource({ title: 'T', url, apps: [{ ...app,
      head: { commit: 'abc1234', date: '2026-10-11T10:00:00+02:00', subject: 'Fix it' },
      built: { commit: 'abc1234', buildNo: 'abc1234', version: '1.2', name: 'Apple', size: 42, permissions: perms } }] });
    const a = doc.apps[0];
    assert.equal(doc.name, 'T');
    assert.equal(a.name, 'Apple');
    assert.deepEqual(a.versions[0], { version: '1.2', buildVersion: 'abc1234', date: '2026-10-11T10:00:00+02:00',
      localizedDescription: 'Fix it', downloadURL: 'https://h/altsource/com.example.a/abc1234.ipa?t=x', size: 42 });
    assert.deepEqual(a.appPermissions, perms);
    assert.equal(a.downloadURL, a.versions[0].downloadURL); // legacy layout
  });

  test('an unbuilt head offers its SHA, with the previous build\'s version and permissions', () => {
    const doc = buildSource({ title: 'T', url, apps: [{ ...app,
      head: { commit: 'def5678', date: 'd', subject: 's' },
      built: { commit: 'abc1234', buildNo: 'abc1234-dirty', version: '1.2', size: 42, permissions: perms } }] });
    const v = doc.apps[0].versions[0];
    assert.equal(v.buildVersion, 'def5678');
    assert.equal(v.version, '1.2');
    assert.match(v.downloadURL, /def5678\.ipa/);
    assert.deepEqual(doc.apps[0].appPermissions, perms);
  });

  test('never built: version 1.0, empty permissions; no head: left out', () => {
    const doc = buildSource({ title: 'T', url, apps: [{ ...app, head: { commit: 'a1b2c3d', date: 'd' }, built: null }, { ...app, slug: 'x', head: null }] });
    assert.equal(doc.apps.length, 1);
    assert.equal(doc.apps[0].versions[0].version, '1.0');
    assert.deepEqual(doc.apps[0].appPermissions, { entitlements: [], privacy: {} });
  });
});

describe('requests', () => {
  function harness() {
    const dir = fs.mkdtempSync(path.join(TMP, 'store-'));
    const jobs = new Map();
    const waiters = new Map();
    let nextId = 1;
    const enqueued = [];
    const s = createAltSource({
      dir,
      apps: () => [{ slug: 'com.example.a', repo: 'A', bundleId: 'com.example.a', platform: 'iOS', branch: 'main' }],
      allApps: () => [{ slug: 'com.example.a', repo: 'A', bundleId: 'com.example.a', platform: 'iOS', branch: 'main' }],
      head: async () => ({ commit: 'abc1234', date: 'd', subject: 's' }),
      enqueue: (app, commit) => { const j = { id: nextId++, status: 'queued' }; jobs.set(j.id, j); enqueued.push(commit); return j; },
      job: id => jobs.get(id),
      waitFor: id => new Promise(r => waiters.set(id, r)),
      inspect: async () => ({ name: 'A', version: '1.0', buildNo: 'abc1234', permissions: { entitlements: [], privacy: {} }, icon: Buffer.from('png') }),
    });
    const ipa = path.join(dir, 'in.ipa');
    fs.writeFileSync(ipa, 'ipa');
    const finish = async (id, status, ok) => {
      if (ok) await s.record('com.example.a', 'abc1234', ipa);
      jobs.get(id).status = status; jobs.get(id).error = status === 'failed' ? 'boom' : undefined;
      waiters.get(id)?.(); await new Promise(r => setImmediate(r));
    };
    return { s, jobs, enqueued, finish, ipa, dir };
  }

  test('unknown apps are 404', async () => {
    assert.equal((await harness().s.request('nope', 'abc1234')).status, 404);
  });

  test('concurrent requests for one commit share one build, then get the IPA', async () => {
    const h = harness();
    const a = await h.s.request('com.example.a', 'abc1234');
    const b = await h.s.request('com.example.a', 'abc1234');
    assert.equal(a.jobId, 1);
    assert.equal(b.jobId, 1);
    assert.deepEqual(h.enqueued, ['abc1234']);
    assert.equal(h.s.outcome('com.example.a', 'abc1234', 1).jobId, 1);
    h.jobs.get(1).status = 'running';
    await h.finish(1, 'success', true);
    const done = h.s.outcome('com.example.a', 'abc1234', 1);
    assert.equal(fs.readFileSync(h.s.file(done.entry), 'utf8'), 'ipa');
    assert.ok((await h.s.request('com.example.a', 'abc1234')).entry);
    assert.equal(fs.readFileSync(h.s.iconFile('com.example.a'), 'utf8'), 'png');
  });

  test('a failed build is reported, not retried straight away', async () => {
    const h = harness();
    await h.s.request('com.example.a', 'abc1234');
    await h.finish(1, 'failed', false);
    assert.match(h.s.outcome('com.example.a', 'abc1234', 1).error, /build failed: boom/);
    const again = await h.s.request('com.example.a', 'abc1234');
    assert.equal(again.status, 502);
    assert.equal(h.enqueued.length, 1);
  });

  test('an IPA already built for the commit elsewhere is adopted', async () => {
    const h = harness();
    const r = await h.s.request('com.example.a', 'abc1234', { adopt: () => h.ipa });
    assert.ok(r.entry);
    assert.equal(h.enqueued.length, 0);
  });

  test('keeps two builds per app', async () => {
    const h = harness();
    for (const c of ['aaaa111', 'bbbb222', 'cccc333']) await h.s.record('com.example.a', c, h.ipa);
    assert.ok(h.s.cached('com.example.a', 'cccc333'));
    assert.ok(h.s.cached('com.example.a', 'bbbb222'));
    assert.equal(h.s.cached('com.example.a', 'aaaa111'), null);
    assert.ok(!fs.existsSync(path.join(h.dir, 'com.example.a-aaaa111.ipa')));
  });
});

// ---- end to end: real server, a git repo, and a build-and-sign that makes an unsigned IPA ----
let server, base, repo, cookie;
const PASSWORD = 'correct horse';
const freePort = () => new Promise(r => { const s = net.createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => r(p)); }); });
function get(p, headers = {}) {
  return new Promise((resolve, reject) => {
    http.get(base + p, { headers }, res => {
      const chunks = [];
      res.on('data', c => chunks.push(c));
      res.on('end', () => {
        const buf = Buffer.concat(chunks);
        let json = null; try { json = JSON.parse(buf.toString()); } catch {}
        resolve({ status: res.statusCode, headers: res.headers, buf, json });
      });
    }).on('error', reject);
  });
}
const git = (...a) => execFileSync('git', ['-C', repo, ...a], { encoding: 'utf8' }).trim();

before(async () => {
  repo = path.join(TMP, 'repo');
  fs.mkdirSync(repo);
  git('init', '-q', '-b', 'main');
  git('config', 'user.email', 't@example.com'); git('config', 'user.name', 'T');
  fs.writeFileSync(path.join(repo, 'README'), 'one\n');
  fs.writeFileSync(path.join(repo, '.gitignore'), 'build/\n');
  git('add', '.'); git('commit', '-qm', 'First');
  // a dirty main checkout: the feed must still build clean commits
  fs.writeFileSync(path.join(repo, 'README'), 'uncommitted\n');

  const fake = path.join(TMP, 'build-and-sign');
  fs.writeFileSync(fake, `#!/bin/bash
set -e
echo "ARGS: $*"
sleep "\${FAKE_BUILD_SECONDS:-2}"
stamp=$(git rev-parse --short HEAD)
git diff --quiet HEAD -- || stamp="$stamp-dirty"
grep -q uncommitted README && { echo "built the dirty tree" >&2; exit 3; }
app=build/stage/Payload/Fake.app
rm -rf build/stage && mkdir -p "$app"
cat > "$app/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.fake</string>
<key>CFBundleName</key><string>Fake</string>
<key>CFBundleShortVersionString</key><string>2.0</string>
<key>CFBundleVersion</key><string>$stamp</string>
<key>NSCameraUsageDescription</key><string>to see</string>
</dict></plist>
PLIST
(cd build/stage && zip -qr ../Fake-signed.ipa Payload)
echo "PLATFORM=$PLATFORM"
`, { mode: 0o755 });

  const port = await freePort();
  base = `http://127.0.0.1:${port}`;
  fs.writeFileSync(path.join(TMP, 'config.json'), JSON.stringify({
    port, bundlePrefix: 'com.example', buildAndSign: fake, passwordHash: hashPassword(PASSWORD),
    altSource: { enabled: true, name: 'Test Source', holdSeconds: 1 },
    repos: [{ name: 'Fake', path: repo, branches: ['main'],
      platforms: [{ platform: 'iOS', bundleId: 'com.example.fake', device: 'Phone' },
        { platform: 'visionOS', bundleId: 'com.example.fake', device: 'AVP' }] }],
  }));
  server = spawn(process.execPath, [path.join(__dirname, '..', 'server.js')], {
    env: { ...process.env, APPSTORE_CONFIG: path.join(TMP, 'config.json'), APPSTORE_DATA: path.join(TMP, 'data'),
      DEBUGTRACE_KEY_DIR: path.join(TMP, 'keys'), PATH: '/usr/bin:/bin' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('server did not start')), 10000);
    server.stdout.on('data', d => { if (String(d).includes('listening')) { clearTimeout(t); resolve(); } });
    server.on('exit', c => reject(new Error(`server exited ${c}`)));
  });
  const r = await new Promise((resolve, reject) => {
    const body = JSON.stringify({ password: PASSWORD });
    const q = http.request(base + '/api/login', { method: 'POST', headers: { 'Content-Type': 'application/json', 'Content-Length': body.length } }, resolve);
    q.on('error', reject); q.end(body);
  });
  cookie = r.headers['set-cookie'][0].split(';')[0];
});

after(() => { server?.kill(); });

describe('end to end', () => {
  test('the feed needs its token, and the session API hands it out', async () => {
    assert.equal((await get('/altsource/source.json')).status, 403);
    assert.equal((await get('/altsource/source.json?k=wrong')).status, 403);
    assert.equal((await get('/api/altsource')).status, 401);
    const info = await get('/api/altsource', { Cookie: cookie });
    assert.equal(info.json.enabled, true);
    assert.deepEqual(info.json.sources.map(s => s.platform), ['iOS', 'visionOS']);
    assert.match(info.json.sources[1].altstore, /^altstore:\/\/source\?url=.*visionOS\.json/);
    const feed = await get(new URL(info.json.combined.url).pathname + new URL(info.json.combined.url).search);
    assert.equal(feed.status, 200);
    assert.equal(feed.json.name, 'Test Source');
    assert.equal(feed.json.apps.length, 1);
  });

  test('platform sources list their own row; unknown platforms are 404', async () => {
    const info = await get('/api/altsource', { Cookie: cookie });
    const u = new URL(info.json.sources[1].url);
    const feed = await get(u.pathname + u.search);
    assert.equal(feed.json.name, 'Test Source (visionOS)');
    assert.equal(feed.json.apps[0].platform, 'visionOS');
    assert.match(feed.json.apps[0].downloadURL, /\/altsource\/com\.example\.fake-visionOS\//);
    const lower = await get(u.pathname.toLowerCase() + u.search);
    assert.equal(lower.json.name, 'Test Source (visionOS)');
    const none = await get('/altsource/watchOS.json' + u.search);
    assert.equal(none.status, 404);
    assert.deepEqual(none.json.platforms, ['iOS', 'visionOS']);
  });

  test('downloads need a signed link', async () => {
    assert.equal((await get('/altsource/com.example.fake-iOS/abc1234.ipa')).status, 403);
    assert.equal((await get('/altsource/com.example.fake-iOS/abc1234.ipa?t=1.x')).status, 403);
  });

  test('a download builds the listed commit, redirecting while it builds, then serves the IPA', async () => {
    const info = await get('/api/altsource', { Cookie: cookie });
    const u = new URL(info.json.sources[0].url);
    const feed = (await get(u.pathname + u.search)).json;
    const head = git('rev-parse', '--short', 'HEAD');
    const v = feed.apps[0].versions[0];
    assert.equal(v.buildVersion, head);
    const dl = new URL(v.downloadURL);

    let r = await get(dl.pathname + dl.search);
    assert.equal(r.status, 307); // held 1 s, build takes 2
    assert.match(r.headers.location, /[?&]w=1/);
    assert.match(r.headers.location, /[?&]t=/);
    let hops = 1;
    while (r.status === 307 && hops < 20) { r = await get(r.headers.location); hops++; }
    assert.equal(r.status, 200, r.buf.toString());
    assert.equal(r.buf.subarray(0, 2).toString(), 'PK');

    // the listing now carries what the IPA says
    const again = (await get(u.pathname + u.search)).json.apps[0];
    assert.equal(again.name, 'Fake');
    assert.equal(again.versions[0].version, '2.0');
    assert.equal(again.versions[0].buildVersion, head);
    assert.deepEqual(again.appPermissions.privacy, { NSCameraUsageDescription: 'to see' });
    assert.equal(again.versions[0].size, r.buf.length);

    // served straight from the cache next time, and not recorded as installed on the device
    const cached = await get(dl.pathname + dl.search);
    assert.equal(cached.status, 200);
    const state = await get('/api/state', { Cookie: cookie });
    assert.equal(state.json.installs.Phone, undefined);
    assert.equal(state.json.builds.filter(b => b.altSource).length, 1);
  });

  test('a long hold waits for the build in one request', async () => {
    fs.writeFileSync(path.join(repo, 'README'), 'two\n');
    git('commit', '-qam', 'Second');
    fs.writeFileSync(path.join(repo, 'README'), 'uncommitted\n');
    const info = await get('/api/altsource', { Cookie: cookie });
    const u = new URL(info.json.sources[0].url);
    // the head cache holds a branch head for 10 s
    await new Promise(r => setTimeout(r, 10500));
    const v = (await get(u.pathname + u.search)).json.apps[0].versions[0];
    assert.equal(v.buildVersion, git('rev-parse', '--short', 'HEAD'));
    const dl = new URL(v.downloadURL);
    const r = await get(`${dl.pathname}${dl.search}&hold=30`);
    assert.equal(r.status, 200);
  });

  test('a deploy launches only when Launch is on, and MCP picks the kind of launch', async () => {
    const post = (p, body) => new Promise((resolve, reject) => {
      const data = JSON.stringify(body);
      const q = http.request(base + p, { method: 'POST', headers: { Cookie: cookie, 'Content-Type': 'application/json', 'Content-Length': data.length } }, res => {
        const chunks = []; res.on('data', c => chunks.push(c)); res.on('end', () => resolve(JSON.parse(Buffer.concat(chunks).toString())));
      });
      q.on('error', reject); q.end(data);
    });
    const argsOf = async (options) => {
      const { jobs } = await post('/api/build', { repo: 'Fake', targets: [{ device: 'Phone', branch: 'current' }], ...options });
      for (;;) {
        const b = (await get('/api/state', { Cookie: cookie })).json.builds.find(x => x.id === jobs[0].id);
        if (!['queued', 'running'].includes(b.status)) break;
        await new Promise(r => setTimeout(r, 200));
      }
      const log = (await get(`/api/builds/${jobs[0].id}/log`, { Cookie: cookie })).json;
      return log.chunk.match(/ARGS: (.*)/)[1].trim().split(/\s+/);
    };
    const mcpOnly = await argsOf({ mcp: true, run: false });
    assert.ok(mcpOnly.includes('--no-mcp') && !mcpOnly.includes('--run'), mcpOnly.join(' '));
    const debugLaunch = await argsOf({ mcp: true, run: true });
    assert.ok(debugLaunch.includes('--run') && debugLaunch.includes('--mcp'), debugLaunch.join(' '));
    const bareLaunch = await argsOf({ mcp: false, run: true });
    assert.ok(bareLaunch.includes('--no-mcp') && !bareLaunch.includes('--run'), bareLaunch.join(' '));
  });
});
