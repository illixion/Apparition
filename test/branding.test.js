'use strict';
const { test, describe, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const net = require('node:net');
const path = require('node:path');
const { spawn } = require('node:child_process');

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-brand-'));
let server, base;

const freePort = () => new Promise(r => { const s = net.createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => r(p)); }); });

async function start(branding) {
  const port = await freePort();
  fs.writeFileSync(path.join(TMP, 'config.json'), JSON.stringify({ port, bundlePrefix: 'x', repos: [], branding }));
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
  base = `http://127.0.0.1:${port}`;
}
const stop = () => { server?.kill(); server = null; };
const page = async p => (await fetch(base + p)).text();

describe('branding', () => {
  after(stop);
  test('defaults keep the triangle and plain title', async () => {
    await start(undefined);
    const h = await page('/');
    assert.match(h, /<title>App Store<\/title>/);
    assert.match(h, /class="tri"/);
    assert.match(h, /href="\/favicon\.svg"/);
    stop();
  });
  test('org and emoji icon', async () => {
    await start({ org: 'Acme <Co>', icon: '🚀' });
    const h = await page('/');
    assert.match(h, /<title>Acme &lt;Co&gt;'s App Store<\/title>|<title>Acme &lt;Co&gt;&#39;s App Store<\/title>/);
    assert.match(h, /<span class="org">Acme &lt;Co&gt;&#39;s<\/span> <b>App Store<\/b>/);
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
});
