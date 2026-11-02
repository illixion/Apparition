'use strict';
const { test, describe, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const net = require('node:net');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { hashPassword } = require('../lib/auth');

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'store-brand-'));
const PASSWORD = 'correct horse';
let server, base, cookie;

const freePort = () => new Promise(r => { const s = net.createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => r(p)); }); });

// The server runs with a password by default, as it does in real use: with none
// set every visitor counts as signed in and /login redirects away, so the login
// page could not be checked at all. Pass `locked: false` for that open case.
async function start(branding, { locked = true } = {}) {
  const port = await freePort();
  fs.writeFileSync(path.join(TMP, 'config.json'), JSON.stringify({
    port, bundlePrefix: 'x', repos: [], branding, ...(locked ? { passwordHash: hashPassword(PASSWORD) } : {}),
  }));
  server = spawn(process.execPath, [path.join(__dirname, '..', 'server.js')], {
    env: { ...process.env, STORE_CONFIG: path.join(TMP, 'config.json'), STORE_DATA: path.join(TMP, 'data'),
      DEBUGTRACE_KEY_DIR: path.join(TMP, 'keys'), PATH: '/usr/bin:/bin' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('server did not start')), 10000);
    server.stdout.on('data', d => { if (String(d).includes('listening')) { clearTimeout(t); resolve(); } });
    server.on('exit', c => reject(new Error(`server exited ${c}`)));
  });
  base = `http://127.0.0.1:${port}`;
  cookie = null;
  if (locked) {
    const r = await fetch(base + '/api/login', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ password: PASSWORD }) });
    assert.equal(r.status, 200);
    cookie = r.headers.get('set-cookie').split(';')[0];
  }
}
const stop = () => { server?.kill(); server = null; };
// The app pages need the session; the login page is fetched without one, as a
// visitor who is signed out would.
const page = async p => (await fetch(base + p, { headers: p === '/login' || !cookie ? {} : { Cookie: cookie } })).text();

describe('branding', () => {
  after(stop);
  test('defaults keep the triangle and plain title', async () => {
    await start(undefined);
    const h = await page('/');
    assert.match(h, /<title>Apparition Store<\/title>/);
    assert.match(h, /class="tri"/);
    assert.match(h, /href="\/favicon\.svg"/);
    stop();
  });
  test('org and emoji icon', async () => {
    await start({ org: 'Acme <Co>', icon: '🚀' });
    const h = await page('/');
    assert.match(h, /<title>Acme &lt;Co&gt;'s Apparition Store<\/title>|<title>Acme &lt;Co&gt;&#39;s Apparition Store<\/title>/);
    assert.match(h, /<span class="org">Acme &lt;Co&gt;&#39;s<\/span> <b>Apparition Store<\/b>/);
    assert.match(h, /<span class="brandicon">🚀<\/span>/);
    assert.match(h, /data:image\/svg\+xml/);
    assert.doesNotMatch(await page('/login'), /\{\{BRAND/);
    stop();
  });
  test('image path is served and used as favicon; title overrides org', async () => {
    fs.writeFileSync(path.join(TMP, 'logo.svg'), '<svg xmlns="http://www.w3.org/2000/svg"/>');
    await start({ org: 'Acme', title: 'Acme Apps', icon: 'logo.svg' });
    const h = await page('/login');
    assert.match(h, /<title>Sign in · Acme Apps<\/title>/);
    assert.match(h, /<img class="brandicon" src="\/branding\/icon\?v=\d+"/);
    const r = await fetch(base + '/branding/icon');
    assert.equal(r.status, 200);
    assert.equal(r.headers.get('content-type'), 'image/svg+xml');
    stop();
  });
  test('with no password set there is nothing to sign in to, so /login leads to the app', async () => {
    await start({ org: 'Acme' }, { locked: false });
    const r = await fetch(base + '/login', { redirect: 'manual' });
    assert.equal(r.status, 302);
    assert.equal(r.headers.get('location'), '/');
    assert.match(await page('/'), /class="tri"|class="brandicon"/);
    stop();
  });
});
