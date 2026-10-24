'use strict';
// Mac app downloads: the listing, the build-join logic with a fake queue, and the
// whole flow through the real server with a fake build-and-sign that makes a .app.
const { test, describe, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const net = require('node:net');
const { spawn, execFileSync } = require('node:child_process');
const { TMP } = require('./helpers');
const { createMacBuilds, macApps } = require('../lib/macapps');
const { hashPassword } = require('../lib/auth');

describe('listing', () => {
  const config = { repos: [
    { name: 'A', path: '/a', branches: ['dev', 'main'], platforms: [],
      macApps: [{ name: 'A for Mac', bundleId: 'com.example.a.mac' },
        { name: 'A Helper', platform: 'macOS-helper', bundleId: 'com.example.a.helper', branch: 'dev' },
        { name: 'no id' }] },
    { name: 'B', path: '/b', branches: ['trunk'], platforms: [{ platform: 'iOS', bundleId: 'com.example.b', device: 'P' }] },
  ] };

  test('one app per entry with a bundle id, keyed by bundle id and platform', () => {
    const apps = macApps(config);
    assert.deepEqual(apps.map(a => a.slug), ['com.example.a.mac-macOS', 'com.example.a.helper-macOS-helper']);
    assert.equal(apps[0].platform, 'macOS');
    assert.equal(apps[0].name, 'A for Mac');
  });
  test('branch: its own, else main/master, else the first offered', () => {
    const apps = macApps(config);
    assert.equal(apps[0].branch, 'main');
    assert.equal(apps[1].branch, 'dev');
  });
});

describe('requests', () => {
  function harness() {
    const dir = fs.mkdtempSync(path.join(TMP, 'mac-'));
    const jobs = new Map();
    const waiters = new Map();
    let nextId = 1;
    const enqueued = [];
    let bundleId = 'com.example.a';
    const s = createMacBuilds({
      dir,
      apps: () => [{ slug: 'com.example.a-macOS', repo: 'A', bundleId: 'com.example.a', platform: 'macOS', branch: 'main' }],
      enqueue: (app, commit, branch) => { const j = { id: nextId++, status: 'queued' }; jobs.set(j.id, j); enqueued.push(`${branch}@${commit}`); return j; },
      job: id => jobs.get(id),
      waitFor: id => new Promise(r => waiters.set(id, r)),
      inspect: async () => ({ name: 'A', bundleId, version: '1.0', buildNo: 'abc1234', minOSVersion: '14.0' }),
      pack: async (app, dest) => { fs.writeFileSync(dest, 'zip'); return 'A.app'; },
    });
    const finish = async (id, status, ok) => {
      if (ok) await s.record('com.example.a-macOS', 'abc1234', 'main', '/x/A-macOS.app');
      jobs.get(id).status = status; jobs.get(id).error = status === 'failed' ? 'boom' : undefined;
      waiters.get(id)?.(); await new Promise(r => setImmediate(r));
    };
    return { s, jobs, enqueued, finish, dir, setBundle: b => { bundleId = b; } };
  }

  test('unknown apps are 404', () => {
    assert.equal(harness().s.request('nope', 'abc1234', 'main').status, 404);
  });

  test('concurrent requests for one commit share one build, then get the zip', async () => {
    const h = harness();
    assert.equal(h.s.request('com.example.a-macOS', 'abc1234', 'main').jobId, 1);
    assert.equal(h.s.request('com.example.a-macOS', 'abc1234', 'main').jobId, 1);
    assert.deepEqual(h.enqueued, ['main@abc1234']);
    assert.deepEqual(h.s.list('A')[0].building, [{ commit: 'abc1234', jobId: 1 }]);
    await h.finish(1, 'success', true);
    const done = h.s.outcome('com.example.a-macOS', 'abc1234', 1);
    assert.equal(fs.readFileSync(h.s.file(done.entry), 'utf8'), 'zip');
    assert.equal(done.entry.bundle, 'A.app');
    assert.ok(h.s.request('com.example.a-macOS', 'abc1234', 'main').entry);
    const listed = h.s.list('A')[0];
    assert.equal(listed.building.length, 0);
    assert.equal(listed.built[0].commit, 'abc1234');
    assert.equal(listed.built[0].file, undefined);
  });

  test('a failed build is reported, not retried straight away', async () => {
    const h = harness();
    h.s.request('com.example.a-macOS', 'abc1234', 'main');
    await h.finish(1, 'failed', false);
    assert.match(h.s.outcome('com.example.a-macOS', 'abc1234', 1).error, /build failed: boom/);
    assert.equal(h.s.request('com.example.a-macOS', 'abc1234', 'main').status, 502);
    assert.equal(h.enqueued.length, 1);
  });

  test('an app with another bundle id is refused, not cached', async () => {
    const h = harness();
    h.setBundle('com.example.other');
    await assert.rejects(h.s.record('com.example.a-macOS', 'abc1234', 'main', '/x/A-macOS.app'), /produced com\.example\.other, not com\.example\.a/);
    assert.equal(h.s.cached('com.example.a-macOS', 'abc1234'), null);
    assert.deepEqual(fs.readdirSync(h.dir).filter(f => f.endsWith('.zip')), []);
  });

  test('keeps three builds per app', async () => {
    const h = harness();
    for (const c of ['aaaa111', 'bbbb222', 'cccc333', 'dddd444']) await h.s.record('com.example.a-macOS', c, 'main', '/x/A-macOS.app');
    assert.ok(h.s.cached('com.example.a-macOS', 'dddd444'));
    assert.ok(h.s.cached('com.example.a-macOS', 'bbbb222'));
    assert.equal(h.s.cached('com.example.a-macOS', 'aaaa111'), null);
    assert.ok(!fs.existsSync(path.join(h.dir, 'com.example.a-macOS-aaaa111.app.zip')));
  });
});

// ---- end to end: real server, a git repo, and a build-and-sign that makes an unsigned .app ----
let server, base, repo, cookie;
const PASSWORD = 'correct horse';
const SLUG = 'com.example.fake.mac-macOS';
const freePort = () => new Promise(r => { const s = net.createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => r(p)); }); });
function request(method, p, { headers = {}, body } = {}) {
  return new Promise((resolve, reject) => {
    const data = body === undefined ? null : JSON.stringify(body);
    const q = http.request(base + p, { method, headers: { ...headers, ...(data ? { 'Content-Type': 'application/json', 'Content-Length': data.length } : {}) } }, res => {
      const chunks = [];
      res.on('data', c => chunks.push(c));
      res.on('end', () => {
        const buf = Buffer.concat(chunks);
        let json = null; try { json = JSON.parse(buf.toString()); } catch {}
        resolve({ status: res.statusCode, headers: res.headers, buf, json });
      });
    });
    q.on('error', reject);
    q.end(data);
  });
}
const get = (p, headers) => request('GET', p, { headers });
const post = (p, body) => request('POST', p, { headers: { Cookie: cookie }, body });
const git = (...a) => execFileSync('git', ['-C', repo, ...a], { encoding: 'utf8' }).trim();
async function settle(id) {
  for (;;) {
    const b = (await get('/api/state', { Cookie: cookie })).json.builds.find(x => x.id === id);
    if (!['queued', 'running'].includes(b.status)) return b;
    await new Promise(r => setTimeout(r, 200));
  }
}

before(async () => {
  repo = path.join(TMP, 'repo');
  fs.mkdirSync(repo);
  git('init', '-q', '-b', 'main');
  git('config', 'user.email', 't@example.com'); git('config', 'user.name', 'T');
  fs.writeFileSync(path.join(repo, 'README'), 'one\n');
  fs.writeFileSync(path.join(repo, '.gitignore'), 'build/\n');
  git('add', '.'); git('commit', '-qm', 'First');
  // a dirty main checkout: downloads must still build the clean commit
  fs.writeFileSync(path.join(repo, 'README'), 'uncommitted\n');
  // an iOS IPA left in build/ by an earlier deploy must not be published for the Mac app
  fs.mkdirSync(path.join(repo, 'build'));
  fs.writeFileSync(path.join(repo, 'build', 'Other-signed.ipa'), 'not an ipa');

  const fake = path.join(TMP, 'build-and-sign');
  fs.writeFileSync(fake, `#!/bin/bash
set -e
echo "ARGS: $*"
sleep "\${FAKE_BUILD_SECONDS:-1}"
stamp=$(git rev-parse --short HEAD)
git diff --quiet HEAD -- || stamp="$stamp-dirty"
grep -q uncommitted README && { echo "built the dirty tree" >&2; exit 3; }
id=com.example.fake.mac
[[ "$PLATFORM" == macOS-wrong ]] && id=com.example.fake.ios
app=build/Fake-macOS.app
rm -rf "$app" && mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks/Lib.framework/Versions/A"
ln -s A "$app/Contents/Frameworks/Lib.framework/Versions/Current"
echo bin > "$app/Contents/MacOS/Fake"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>$id</string>
<key>CFBundleName</key><string>Fake</string>
<key>CFBundleShortVersionString</key><string>3.0</string>
<key>CFBundleVersion</key><string>$stamp</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
</dict></plist>
PLIST
echo "PLATFORM=$PLATFORM"
printf '  %-12s %s\\n' "App path:" "$PWD/$app"
`, { mode: 0o755 });

  const port = await freePort();
  base = `http://127.0.0.1:${port}`;
  fs.writeFileSync(path.join(TMP, 'config.json'), JSON.stringify({
    port, bundlePrefix: 'com.example', buildAndSign: fake, passwordHash: hashPassword(PASSWORD),
    repos: [
      { name: 'Fake', path: repo, branches: ['main'],
        platforms: [{ platform: 'iOS', bundleId: 'com.example.fake', device: 'Phone' }],
        macApps: [{ name: 'Fake for Mac', bundleId: 'com.example.fake.mac' },
          { name: 'Misconfigured', platform: 'macOS-wrong', bundleId: 'com.example.fake.helper' }] },
      { name: 'MacOnly', path: repo, branches: ['main'], platforms: [] },
    ],
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
  const r = await request('POST', '/api/login', { body: { password: PASSWORD } });
  cookie = r.headers['set-cookie'][0].split(';')[0];
});

after(() => { server?.kill(); });

describe('end to end', () => {
  test('the download button builds the clean head, then hands out the zip', async () => {
    const head = git('rev-parse', '--short', 'HEAD');
    const first = await post(`/api/mac/${SLUG}/build`, { branch: 'current' });
    assert.equal(first.status, 200, first.buf.toString());
    assert.equal(first.json.ready, false);
    assert.equal(first.json.commit, head);
    assert.equal(first.json.branch, 'main');
    const b = await settle(first.json.jobId);
    assert.equal(b.status, 'success', b.error);
    assert.equal(b.macApp, SLUG);
    assert.equal(b.buildNo, head);
    const log = (await get(`/api/builds/${b.id}/log`, { Cookie: cookie })).json.chunk;
    assert.match(log, /ARGS: --no-deploy\s*$/m);
    assert.match(log, /PLATFORM=macOS$/m);

    const ready = await post(`/api/mac/${SLUG}/build`, { branch: 'main' });
    assert.equal(ready.json.ready, true);
    const zip = await get(ready.json.url, { Cookie: cookie });
    assert.equal(zip.status, 200);
    assert.equal(zip.headers['content-type'], 'application/zip');
    assert.match(zip.headers['content-disposition'], new RegExp(`filename="Fake-${head}\\.app\\.zip"`));

    // the zip holds the app under its product name, symlinks intact
    const out = fs.mkdtempSync(path.join(TMP, 'unzip-'));
    fs.writeFileSync(path.join(out, 'a.zip'), zip.buf);
    execFileSync('ditto', ['-x', '-k', path.join(out, 'a.zip'), out]);
    assert.ok(fs.existsSync(path.join(out, 'Fake.app', 'Contents', 'Info.plist')));
    assert.ok(fs.lstatSync(path.join(out, 'Fake.app', 'Contents', 'Frameworks', 'Lib.framework', 'Versions', 'Current')).isSymbolicLink());

    // listed on the page; no install recorded and no OTA copy published
    const state = (await get('/api/state', { Cookie: cookie })).json;
    const mac = state.repos.find(r => r.name === 'Fake').macApps[0];
    assert.equal(mac.built[0].commit, head);
    assert.equal(mac.built[0].version, '3.0');
    assert.deepEqual(state.installs, {});
    assert.deepEqual(state.ota, {});
  });

  test('downloads need the session or a signed link; a latest link follows the branch', async () => {
    assert.equal((await get(`/mac/${SLUG}/latest.app.zip?branch=main`)).status, 302); // to the sign-in page
    assert.match((await get(`/mac/${SLUG}/latest.app.zip?branch=main`)).headers.location, /^\/login/);
    const link = (await get(`/api/mac/${SLUG}/link?branch=main`, { Cookie: cookie })).json;
    assert.match(link.curl, /^curl -fL .*&hold=600' && ditto -x -k 'FakeforMac\.app\.zip' \.$/);
    const u = new URL(link.url);
    const r = await get(u.pathname + u.search);
    assert.equal(r.status, 302);
    assert.match(r.headers.location, new RegExp(`^/mac/${SLUG}/${git('rev-parse', '--short', 'HEAD')}\\.app\\.zip\\?branch=main&t=`));
    assert.equal((await get(r.headers.location)).status, 200);
    // the signature covers the branch
    assert.equal((await get(u.pathname + u.search.replace('branch=main', 'branch=dev'))).status, 403);
    assert.equal((await get(`/mac/${SLUG}/latest.app.zip?branch=main&t=1.x`)).status, 403);
  });

  test('only offered branches build', async () => {
    const r = await post(`/api/mac/${SLUG}/build`, { branch: 'nope' });
    assert.equal(r.status, 400);
    assert.match(r.json.error, /does not offer branch nope/);
  });

  test('a conf alias that builds another app fails the build instead of serving it', async () => {
    const slug = 'com.example.fake.helper-macOS-wrong';
    const first = await post(`/api/mac/${slug}/build`, { branch: 'main' });
    const b = await settle(first.json.jobId);
    assert.equal(b.status, 'failed');
    assert.match(b.error, /produced com\.example\.fake\.ios, not com\.example\.fake\.helper/);
    const again = await post(`/api/mac/${slug}/build`, { branch: 'main' });
    assert.equal(again.status, 502);
  });

  test('a repo with only Mac apps has nothing to deploy', async () => {
    const state = (await get('/api/state', { Cookie: cookie })).json;
    assert.deepEqual(state.repos.find(r => r.name === 'MacOnly').platforms, []);
    const r = await post('/api/build', { repo: 'MacOnly', targets: [{ device: 'Phone', branch: 'main' }] });
    assert.equal(r.status, 500);
    assert.match(r.json.error, /no device platforms/);
  });
});
