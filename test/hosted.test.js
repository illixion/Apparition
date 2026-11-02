'use strict';
// The server as the Apparition desktop app runs it: APPSTORE_HOSTED=1, a token on stdin, the web
// pages off until turned on with a password, and the server ending when the app's pipe closes.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const net = require('node:net');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');
const { once } = require('node:events');

const SERVER = path.join(__dirname, '..', 'server.js');

async function freePort() {
  const socket = net.createServer().listen(0, '127.0.0.1');
  await once(socket, 'listening');
  const { port } = socket.address();
  await new Promise(resolve => socket.close(resolve));
  return port;
}

async function hosted() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-hosted-'));
  const port = await freePort();
  fs.writeFileSync(path.join(dir, 'config.json'), JSON.stringify({ port, bundlePrefix: 'com.example', repos: [] }));
  const token = crypto.randomBytes(32).toString('base64url');
  const child = spawn(process.execPath, [SERVER], {
    env: { ...process.env, APPSTORE_HOSTED: '1', APPSTORE_CONFIG: path.join(dir, 'config.json'),
      APPSTORE_DATA: path.join(dir, 'data'), DEBUGTRACE_KEY_DIR: path.join(dir, 'keys'), PATH: '/nonexistent' },
    stdio: ['pipe', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', d => { output += d; });
  child.stderr.on('data', d => { output += d; });
  child.stdin.write(token + '\n');
  const base = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 100 && !output.includes('listening'); i++) await new Promise(r => setTimeout(r, 50));
  assert.match(output, /listening/, output);
  const call = (route, { method = 'GET', body, bearer = token } = {}) => fetch(base + route, { method, redirect: 'manual',
    headers: { 'Content-Type': 'application/json', ...(bearer ? { Authorization: `Bearer ${bearer}` } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body) });
  return { child, call, dir, token };
}

test('hosted: the app reaches the service by its token, and the web pages are off', async t => {
  const { child, call } = await hosted();
  t.after(() => child.kill());
  assert.equal((await call('/api/service/v1/state')).status, 200);
  assert.equal((await call('/api/service/v1/state', { bearer: crypto.randomBytes(32).toString('base64url') })).status, 401);
  const host = await (await call('/api/service/v1/host')).json();
  assert.deepEqual([host.hosted, host.webUi, host.passwordSet, host.webUiOn], [true, false, false, false]);
  for (const route of ['/', '/login', '/api/state']) {
    const reply = await call(route, { bearer: null });
    assert.equal(reply.status, 404, route);
    assert.match(await reply.text(), /web UI is off/);
  }
  // A device's signed upload still gets through to its own check.
  assert.notEqual((await call('/api/traces', { method: 'POST', bearer: null, body: {} })).status, 404);
});

test('hosted: the web UI turns on only with a password, which then signs browsers and devices in', async t => {
  const { child, call, dir } = await hosted();
  t.after(() => child.kill());
  const refused = await call('/api/service/v1/host/web-ui', { method: 'POST', body: { enabled: true } });
  assert.equal(refused.status, 400);
  const on = await call('/api/service/v1/host/web-ui', { method: 'POST', body: { enabled: true, password: 'correct horse' } });
  assert.equal(on.status, 200);
  assert.equal((await on.json()).webUiOn, true);
  assert.equal(JSON.parse(fs.readFileSync(path.join(dir, 'config.json'), 'utf8')).webUi, true);
  assert.equal((await call('/api/state', { bearer: null })).status, 401, 'a browser still has to sign in');
  const login = await call('/api/login', { method: 'POST', bearer: null, body: { password: 'correct horse' } });
  assert.equal(login.status, 200);
  const cookie = login.headers.get('set-cookie').split(';')[0];
  assert.equal((await fetch(login.url.replace('/api/login', '/api/state'), { headers: { Cookie: cookie } })).status, 200);
  const device = await call('/api/service/connect', { method: 'POST', bearer: null, body: { password: 'correct horse' } });
  assert.equal(device.status, 200);
  // Only the host reaches the host routes, not a device session.
  const session = (await device.json()).token;
  assert.equal((await call('/api/service/v1/host', { bearer: session })).status, 404);
  const off = await call('/api/service/v1/host/web-ui', { method: 'POST', body: { enabled: false } });
  assert.equal((await off.json()).webUiOn, false);
  assert.equal((await call('/', { bearer: null })).status, 404);
});

test('hosted: the server ends when the app closes its pipe', async () => {
  const { child } = await hosted();
  child.stdin.end();
  const [code] = await once(child, 'exit');
  assert.equal(code, 0);
});
