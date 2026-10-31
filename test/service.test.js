'use strict';
const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const crypto = require('node:crypto');
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
    passwordHash: hashPassword(password), crashLogs: { enabled: false }, buildManifestDir: 'artifacts' }));
  child = spawn(process.execPath, [path.join(__dirname, '..', 'server.js')], {
    env: { ...process.env, APPSTORE_CONFIG: config, APPSTORE_DATA: path.join(dir, 'data'),
      DEBUGTRACE_KEY_DIR: path.join(dir, 'keys'), BUILD_SIGNING_DIR: path.join(dir, 'signing') }, stdio: ['ignore', 'pipe', 'pipe'],
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

test('project registration requires a native session and immediately updates state without losing config', async () => {
  const project = path.join(dir, 'Registered'); fs.mkdirSync(project);
  const { execFileSync } = require('node:child_process');
  execFileSync('git', ['init', '-b', 'main', project], { stdio: 'ignore' });
  fs.writeFileSync(path.join(project, 'build-signing.conf'), 'exit 77 # not executed\n');
  const body = { name: 'Registered', path: project, branches: [], platforms: [
    { platform: 'visionOS', bundleId: 'com.example.registered', device: 'AVP' },
  ] };
  assert.equal((await request('/api/service/v1/projects', { method: 'POST', body })).status, 401);
  const token = await connect();
  const added = await request('/api/service/v1/projects', { method: 'POST', token, body });
  assert.equal(added.status, 200); assert.equal(added.body.project.name, 'Registered');
  const state = await request('/api/service/v1/state', { token });
  assert.ok(state.body.repos.some(repo => repo.name === 'Registered'));
  assert.equal((await request('/api/service/v1/projects', { method: 'POST', token, body })).status, 409);
  const persisted = JSON.parse(fs.readFileSync(path.join(dir, 'config.json')));
  assert.ok(persisted.passwordHash); assert.equal(persisted.buildManifestDir, 'artifacts');
  assert.equal(persisted.repos.length, 1);
  // Atomic registration replaces the inode. The watcher must still notice
  // later external edits, including a second replacement of that file.
  for (const mcp of [false, true]) {
    const config = path.join(dir, 'config.json'), temporary = config + '.external';
    const next = { ...JSON.parse(fs.readFileSync(config)), deployDefaults: { mcp } };
    fs.writeFileSync(temporary, JSON.stringify(next)); fs.renameSync(temporary, config);
    let reloaded = false;
    for (let attempt = 0; attempt < 30; ++attempt) {
      const current = await request('/api/service/v1/state', { token });
      if (current.body.repos[0].deployDefaults.mcp === mcp) { reloaded = true; break; }
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.equal(reloaded, true, 'configuration replacement must remain watched');
  }
});

test('artifact manifests and exact downloads require the native session', async () => {
  const buildId = 'fixture-build', content = Buffer.from('immutable test artifact');
  const sha256 = crypto.createHash('sha256').update(content).digest('hex');
  const artifact = { sha256, bytes: content.length, kind: 'ipa', file: `${sha256}.ipa` };
  const directory = path.join(dir, 'artifacts', buildId); fs.mkdirSync(directory, { recursive: true });
  fs.writeFileSync(path.join(directory, artifact.file), content);
  fs.writeFileSync(path.join(directory, 'manifest.json'), JSON.stringify({ format: 'apparition-build/1', buildId, artifact }));
  assert.equal((await request(`/api/service/v1/artifacts/${buildId}/manifest`)).status, 401);
  const token = await connect();
  const listed = await request('/api/service/v1/artifacts', { token });
  assert.equal(listed.status, 200); assert.equal(listed.body.builds[0].buildId, buildId);
  const manifest = await request(`/api/service/v1/artifacts/${buildId}/manifest`, { token });
  assert.equal(manifest.status, 200); assert.equal(manifest.body.artifact.sha256, sha256);
  const download = await fetch(base + `/api/service/v1/artifacts/${buildId}/download`, { headers: { Authorization: `Bearer ${token}` } });
  assert.equal(download.status, 200); assert.deepEqual(Buffer.from(await download.arrayBuffer()), content);
  assert.equal((await request('/api/service/v1/artifacts/nonexistent/manifest', { token })).status, 404);
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

test('re-signing is bounded: it needs a session, a kept build, a configured target and a known profile', async () => {
  assert.equal((await request('/api/service/v1/resign/options')).status, 401);
  assert.equal((await request('/api/service/v1/resign', { method: 'POST', body: {} })).status, 401);
  const token = await connect();
  assert.ok((await request('/api/service', {})).body.capabilities.includes('resign'));
  const options = await request('/api/service/v1/resign/options', { token });
  assert.equal(options.status, 200); assert.deepEqual(options.body.profiles, []);
  const post = body => request('/api/service/v1/resign', { method: 'POST', token, body });
  for (const bad of [{}, { buildId: 'x', device: 'AVP' }, { buildId: 'x', device: 'AVP', profile: 'p', dataMode: 'wipe' },
    { buildId: 'x', device: 'AVP', profile: 'p', dryRun: 'yes' }, { buildId: 'x', device: 7, profile: 'p' }]) {
    assert.equal((await post(bad)).status, 400, JSON.stringify(bad));
  }
  assert.equal((await post({ buildId: 'no-such-build', device: 'AVP', profile: 'p' })).status, 404);
  // A kept build whose app no configured project installs on that device has no signing setup.
  const buildId = 'resign-fixture', content = Buffer.from('ipa bytes');
  const sha256 = crypto.createHash('sha256').update(content).digest('hex');
  const artifact = { sha256, bytes: content.length, kind: 'ipa', file: `${sha256}.ipa` };
  const directory = path.join(dir, 'artifacts', buildId); fs.mkdirSync(directory, { recursive: true });
  fs.writeFileSync(path.join(directory, artifact.file), content);
  fs.writeFileSync(path.join(directory, 'manifest.json'), JSON.stringify({ format: 'apparition-build/1', buildId, platform: 'visionOS',
    app: { bundleId: 'com.example.app', name: 'Example' }, artifact }));
  const refused = await post({ buildId, device: 'AVP', profile: 'p' });
  assert.equal(refused.status, 400); assert.match(refused.body.error, /No configured project installs com\.example\.app on AVP/);
});
