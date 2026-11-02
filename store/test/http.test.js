'use strict';
// The real server in a child process, with a password set, a fake discovery
// tool and a fake app, to check what each route lets through.
const { test, describe, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const net = require('node:net');
const http = require('node:http');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { hashPassword, COOKIE } = require('../lib/auth');

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-http-'));
const KEYS = path.join(TMP, 'keys');
const PASSWORD = 'correct horse';
let server, base, fakeApp, fakeAppUrl, calls = [];

const freePort = () => new Promise(r => { const s = net.createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => r(p)); }); });

function req(method, p, { body, cookie, headers = {} } = {}) {
  return new Promise((resolve, reject) => {
    const data = body === undefined ? null : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
    const r = http.request(base + p, { method, headers: { ...(data ? { 'Content-Type': 'application/json', 'Content-Length': data.length } : {}), ...(cookie ? { Cookie: cookie } : {}), ...headers } }, res => {
      const chunks = [];
      res.on('data', c => chunks.push(c));
      res.on('end', () => {
        const text = Buffer.concat(chunks).toString();
        let json = null; try { json = JSON.parse(text); } catch {}
        resolve({ status: res.statusCode, headers: res.headers, text, json });
      });
    });
    r.on('error', reject);
    if (data) r.write(data);
    r.end();
  });
}

async function login() {
  const r = await req('POST', '/api/login', { body: { password: PASSWORD } });
  assert.equal(r.status, 200);
  return r.headers['set-cookie'][0].split(';')[0];
}

before(async () => {
  // The fake app: answers /_call like DebugTraceServer, and checks the token.
  fakeApp = http.createServer((q, s) => {
    let body = '';
    q.on('data', c => body += c);
    q.on('end', () => {
      calls.push({ path: q.url, auth: q.headers.authorization, body: body && JSON.parse(body) });
      if (q.headers.authorization !== 'Bearer app-token') { s.writeHead(401, { 'Content-Type': 'application/json' }); return s.end('{"ok":false}'); }
      s.writeHead(200, { 'Content-Type': 'application/json' });
      const b = body && JSON.parse(body);
      if (b?.name === '_logs') {
        const after = b.arguments.afterSequence;
        const entries = after === 0 ? [{ time: '2026-10-06T12:00:00.000Z', level: 'info', subsystem: 's', category: 'App', message: 'hello', sequence: 1 }] : [];
        if (after > 0) return setTimeout(() => s.end(JSON.stringify({ ok: true, data: { entries, lastSequence: after, truncated: false } })), 300);
        return s.end(JSON.stringify({ ok: true, data: { entries, lastSequence: 1, truncated: false } }));
      }
      s.end(JSON.stringify({ ok: true, endpoint: b?.name, data: { echo: b?.arguments ?? null } }));
    });
  });
  await new Promise(r => fakeApp.listen(0, '127.0.0.1', r));
  fakeAppUrl = `http://127.0.0.1:${fakeApp.address().port}`;

  fs.mkdirSync(KEYS);
  fs.writeFileSync(path.join(KEYS, 'k1.json'), JSON.stringify({ keyId: 'k1', commandToken: 'app-token' }));
  fs.writeFileSync(path.join(KEYS, 'k2.json'), JSON.stringify({ keyId: 'k2' }));
  const tool = path.join(TMP, 'discover');
  fs.writeFileSync(tool, `#!/bin/sh\ncat <<'JSON'\n${JSON.stringify([
    { appName: 'Fake', bundleId: 'pro.example.fake', build: 'abc1234', device: 'Phone', url: fakeAppUrl, keyId: 'k1', source: 'bonjour', attachable: true },
    { appName: 'NoToken', bundleId: 'pro.example.nt', url: 'http://127.0.0.1:9', keyId: 'k2', source: 'bonjour', attachable: false, reason: 'no command token' },
  ])}\nJSON\n`, { mode: 0o755 });

  const port = await freePort();
  base = `http://127.0.0.1:${port}`;
  fs.writeFileSync(path.join(TMP, 'config.json'), JSON.stringify({
    port, bundlePrefix: 'pro.example', repos: [], passwordHash: hashPassword(PASSWORD), debugtraceTool: tool,
  }));
  server = spawn(process.execPath, [path.join(__dirname, '..', 'server.js')], {
    env: { ...process.env, APPSTORE_CONFIG: path.join(TMP, 'config.json'), APPSTORE_DATA: path.join(TMP, 'data'),
      DEBUGTRACE_KEY_DIR: KEYS, PATH: '/usr/bin:/bin' }, // no xcrun: device refreshes fail fast
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('server did not start')), 10000);
    server.stdout.on('data', d => { if (String(d).includes('listening')) { clearTimeout(t); resolve(); } });
    server.on('exit', c => reject(new Error(`server exited ${c}`)));
  });
});

after(() => { server?.kill(); fakeApp?.close(); });

describe('without a session', () => {
  test('API calls get 401 and pages redirect to the login page', async () => {
    assert.equal((await req('GET', '/api/state')).status, 401);
    assert.equal((await req('POST', '/api/build', { body: {} })).status, 401);
    assert.equal((await req('GET', '/api/debug/apps')).status, 401);
    const page = await req('GET', '/');
    assert.equal(page.status, 302);
    assert.equal(page.headers.location, '/login?next=%2F');
    assert.equal((await req('GET', '/ota')).status, 302);
  });
  test('the login page and icons load', async () => {
    assert.equal((await req('GET', '/login')).status, 200);
    assert.equal((await req('GET', '/favicon.svg')).status, 200);
    assert.equal((await req('GET', '/freshness.js')).status, 302);
  });
  test('trace uploads still reach the signature check', async () => {
    const r = await req('POST', '/api/traces', { body: 'not a zip', headers: { 'Content-Type': 'application/zip' } });
    assert.equal(r.status, 400);
  });
  test('OTA files need a valid signed link', async () => {
    assert.equal((await req('GET', '/ota/x/manifest.plist')).status, 302);
    assert.equal((await req('GET', '/ota/x.ipa?t=123.abc')).status, 302);
  });
  test('a wrong password is refused', async () => {
    const r = await req('POST', '/api/login', { body: { password: 'nope' } });
    assert.equal(r.status, 401);
    assert.equal(r.headers['set-cookie'], undefined);
  });
});

describe('with a session', () => {
  test('the page and API work, and the cookie is locked down', async () => {
    const r = await req('POST', '/api/login', { body: { password: PASSWORD } });
    assert.match(r.headers['set-cookie'][0], /HttpOnly; SameSite=Strict/);
    const cookie = r.headers['set-cookie'][0].split(';')[0];
    const state = await req('GET', '/api/state', { cookie });
    assert.equal(state.status, 200);
    assert.equal(state.json.authRequired, true);
    assert.equal((await req('GET', '/', { cookie })).status, 200);
  });
  test('the login page sends a signed-in visitor on instead of asking again', async () => {
    const cookie = await login();
    const r = await req('GET', '/login', { cookie });
    assert.equal(r.status, 302);
    assert.equal(r.headers.location, '/');
    assert.equal(r.headers['cache-control'], 'no-store'); // a cached redirect would hide the form after the session ends
    // the page a visitor was sent from stays where they land
    assert.equal((await req('GET', '/login?next=%2Fota', { cookie })).headers.location, '/ota');
    assert.equal((await req('GET', '/login?next=%2F%23debug%2Fk1', { cookie })).headers.location, '/#debug/k1');
  });
  test('the login page never redirects a signed-in visitor off this site', async () => {
    const cookie = await login();
    for (const next of ['//evil.example', 'https://evil.example', '/\\evil.example', '\\\\evil.example', 'evil.example',
      '/ok\r\nSet-Cookie: x=1', '/ok\nX: y', '/ok\u0000', 'javascript:alert(1)', '']) {
      const r = await req('GET', `/login?next=${encodeURIComponent(next)}`, { cookie });
      assert.equal(r.status, 302, JSON.stringify(next));
      assert.equal(r.headers.location, '/', JSON.stringify(next));
      assert.equal(r.headers['set-cookie'], undefined, JSON.stringify(next));
    }
  });
  test('an expired or forged session still gets the login page', async () => {
    const forged = `${COOKIE}=${'0'.repeat(64)}`;
    assert.equal((await req('GET', '/login', { cookie: forged })).status, 200);
    assert.equal((await req('GET', '/login?next=%2Fota', { cookie: forged })).status, 200);
    assert.equal((await req('GET', '/api/state', { cookie: forged })).status, 401); // and it really is not a session
  });
  test('cross-site requests are refused even with the cookie', async () => {
    const cookie = await login();
    const r = await req('POST', '/api/refresh-devices', { cookie, headers: { Origin: 'https://evil.example' } });
    assert.equal(r.status, 403);
    const same = await req('POST', '/api/refresh-devices', { cookie, headers: { Origin: base } });
    assert.equal(same.status, 200);
  });
  test('logging out ends the session', async () => {
    const cookie = await login();
    await req('POST', '/api/logout', { cookie });
    assert.equal((await req('GET', '/api/state', { cookie })).status, 401);
  });
});

describe('live debugging', () => {
  test('apps are listed without their tokens or URLs', async () => {
    const r = await req('GET', '/api/debug/apps', { cookie: await login() });
    assert.equal(r.status, 200);
    const [fake, noToken] = r.json.apps;
    assert.equal(fake.id, 'k1');
    assert.equal(fake.attachable, true);
    assert.equal(noToken.attachable, false);
    assert.match(r.json.clientName, /^App Store page on /); // what the device's approval prompt shows
    assert.doesNotMatch(r.text, /app-token|127\.0\.0\.1/);
  });
  test('a call reaches the app with the ledger token', async () => {
    calls = [];
    const r = await req('POST', '/api/debug/k1/call', { cookie: await login(), body: { endpoint: '_info', arguments: { a: 1 } } });
    assert.equal(r.status, 200);
    assert.deepEqual(r.json.data.echo, { a: 1 });
    assert.equal(calls[0].auth, 'Bearer app-token');
    assert.deepEqual(calls[0].body, { name: '_info', arguments: { a: 1 } });
  });
  test('unknown apps, tokenless builds and bad endpoint names are refused', async () => {
    const cookie = await login();
    assert.equal((await req('POST', '/api/debug/nope/call', { cookie, body: { endpoint: '_info' } })).status, 404);
    assert.equal((await req('POST', '/api/debug/k2/call', { cookie, body: { endpoint: '_info' } })).status, 403);
    assert.equal((await req('POST', '/api/debug/k1/call', { cookie, body: { endpoint: '../_info' } })).status, 400);
  });
  test('the log stream sends the backlog, then long-polls from the cursor', async () => {
    calls = [];
    const cookie = await login();
    const events = await new Promise((resolve, reject) => {
      const r = http.get(base + '/api/debug/k1/logs?level=info', { headers: { Cookie: cookie } }, res => {
        assert.equal(res.headers['content-type'], 'text/event-stream');
        let text = '';
        res.on('data', c => { text += c; });
        // An idle long poll sends nothing, so watch the app's side instead.
        const check = setInterval(() => {
          if (calls.filter(x => x.body?.name === '_logs').length >= 2 && text.includes('backlog')) {
            clearInterval(check); r.destroy(); resolve(text);
          }
        }, 50);
      });
      r.on('error', e => (e.code === 'ECONNRESET' ? null : reject(e)));
      setTimeout(() => reject(new Error('no stream')), 8000);
    });
    assert.match(events, /event: status\ndata: {"state":"live"}/);
    assert.match(events, /event: backlog\ndata: .*"hello"/);
    const logCalls = calls.filter(x => x.body?.name === '_logs').map(x => x.body.arguments);
    assert.deepEqual(logCalls[0], { level: 'info', format: 'json', afterSequence: 0, limit: 300 });
    assert.equal(logCalls[1].afterSequence, 1);
    assert.equal(logCalls[1].waitSeconds, 20);
  });
  test('an unknown level is refused', async () => {
    assert.equal((await req('GET', '/api/debug/k1/logs?level=loud', { cookie: await login() })).status, 400);
  });
});
