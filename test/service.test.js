'use strict';
const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const { hashPassword } = require('../lib/auth');
const { createService } = require('../lib/service');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-service-test-'));
const password = 'service test password';
let child, base;

before(async () => {
  const socket = net.createServer();
  socket.listen(0, '127.0.0.1'); await once(socket, 'listening');
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  const config = path.join(dir, 'config.json');
  fs.writeFileSync(config, JSON.stringify({ port, bundlePrefix: 'com.example', repos: [],
    passwordHash: hashPassword(password), crashLogs: { enabled: false } }));
  child = spawn(process.execPath, [path.join(__dirname, '..', 'server.js')], {
    env: { ...process.env, APPSTORE_CONFIG: config, APPSTORE_DATA: path.join(dir, 'data'),
      DEBUGTRACE_KEY_DIR: path.join(dir, 'keys') }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('service did not start')), 10000);
    child.stdout.on('data', data => { if (String(data).includes('listening')) { clearTimeout(timer); resolve(); } });
    child.once('exit', code => { clearTimeout(timer); reject(new Error(`service exited ${code}`)); });
  });
  base = `http://127.0.0.1:${port}`;
});
after(async () => {
  if (child && child.exitCode === null) { child.kill('SIGTERM'); await once(child, 'exit'); }
});

async function request(route, { method = 'GET', body, token, headers = {} } = {}) {
  const res = await fetch(base + route, { method, headers: { 'Content-Type': 'application/json',
    ...(token ? { Authorization: `Bearer ${token}` } : {}), ...headers },
    body: body === undefined ? undefined : JSON.stringify(body), redirect: 'manual' });
  return { status: res.status, body: await res.json(), headers: res.headers };
}
async function connect() {
  const result = await request('/api/service/connect', { method: 'POST', body: { password } });
  assert.equal(result.status, 200);
  assert.equal(result.headers.get('set-cookie'), null);
  return result.body.token;
}

test('discovery advertises a versioned API without exposing projects or credentials', async () => {
  const r = await request('/api/service');
  assert.equal(r.status, 200); assert.equal(r.body.protocol, 'apparition-service');
  assert.equal(r.body.version, 1); assert.ok(r.body.capabilities.includes('builds'));
  assert.equal(r.body.token, undefined); assert.equal(r.body.repos, undefined);
});

test('native sessions authorize only the bounded service namespace and are revocable', async () => {
  assert.equal((await request('/api/service/v1/state')).status, 401);
  assert.equal((await request('/api/service/connect', { method: 'POST', body: { password: 'wrong password' } })).status, 401);
  const token = await connect();
  assert.equal((await request('/api/state', { token })).status, 401);
  assert.equal((await request('/api/service/v1/state', { token: 'x'.repeat(43) })).status, 401);
  const r = await request('/api/service/v1/state', { token });
  assert.equal(r.status, 200); assert.deepEqual(r.body.repos, []);
  assert.equal((await request('/api/service/v1/traces', { token })).status, 200);
  assert.equal((await request('/api/service/v1/crashlogs', { token })).status, 200);
  assert.equal((await request('/api/service/v1/builds/42/log?offset=0', { token })).body.status, 'unknown');
  assert.equal((await request('/api/service/v1/login', { method: 'POST', token, body: { password } })).status, 404);
  assert.equal((await request('/api/service/v1/traces/00000000-0000-0000-0000-000000000000/delete', { method: 'POST', token })).status, 404);
  assert.equal((await request('/api/service/v1/build', { method: 'POST', token, body: { repo: 'unknown' } })).status, 400);
  assert.equal((await request('/api/service/v1/disconnect', { method: 'POST', token })).status, 200);
  assert.equal((await request('/api/service/v1/state', { token })).status, 401);
});

test('cross-site requests cannot establish or use a native session', async () => {
  assert.equal((await request('/api/service/connect', { method: 'POST', body: { password },
    headers: { Origin: 'https://unrelated.example' } })).status, 403);
});

test('target validation preserves platform identity and refuses arbitrary branches/options', async () => {
  const config = { passwordHash: 'hash', repos: [{ name: 'Example', branches: ['main'], platforms: [
    { device: 'AVP', platform: 'iPad' }, { device: 'AVP', platform: 'visionOS' },
  ] }] };
  const service = createService({ auth: { check: () => true }, config: () => config,
    readBody: async req => req.body, json: (res, status, body) => Object.assign(res, { status, body }) });
  const req = body => ({ method: 'POST', headers: { authorization: 'Bearer ' + 'a'.repeat(43) }, body });
  const valid = { repo: 'Example', device: 'AVP', targetPlatform: 'visionOS', branch: 'main', run: true };
  const result = await service(req(valid), {}, new URL('http://test/api/service/v1/build'));
  assert.deepEqual(result.body.targets, [{ device: 'AVP', platform: 'visionOS', branch: 'main' }]);
  assert.equal(result.body.run, true);
  for (const bad of [{ device: 'unknown' }, { targetPlatform: 'unknown' }, { branch: 'unconfigured' }, { run: 'yes' }]) {
    const res = {};
    await service(req({ ...valid, ...bad }), res, new URL('http://test/api/service/v1/build'));
    assert.equal(res.status, 400);
  }
});
