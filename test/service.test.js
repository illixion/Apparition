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

test('re-signing without a kept build names a configured project target, and a branch it has', async () => {
  const token = await connect();
  assert.ok((await request('/api/service', {})).body.capabilities.includes('resign-build'));
  const post = body => request('/api/service/v1/resign', { method: 'POST', token, body });
  // 'Registered' (from the registration test) installs on AVP for visionOS and has no branches.
  const cases = [
    [{ repo: 'Nope', device: 'AVP', profile: 'p' }, /Nope has no target on AVP/],
    [{ repo: 'Registered', device: 'ATV', profile: 'p' }, /no target on ATV/],
    [{ repo: 'Registered', device: 'AVP', platform: 'tvOS', profile: 'p' }, /no target on AVP for tvOS/],
    [{ repo: 'Registered', device: 'AVP', branch: '../etc', profile: 'p' }, /not one of Registered's branches/],
    [{ repo: 'Registered', device: 'AVP', profile: 'p' }, /profile is not in the build-signing folder/],
    [{ repo: 7, device: 'AVP', profile: 'p' }, /Choose a kept build or a project/],
  ];
  for (const [body, error] of cases) {
    const reply = await post(body);
    assert.equal(reply.status, 400, JSON.stringify(body)); assert.match(reply.body.error, error);
  }
  const state = await request('/api/service/v1/state', { token });
  assert.ok(!state.body.builds.some(job => job.resign || job.forResign), 'a refused request queues nothing');
});

test('with no kept build, a re-sign builds the project first and signs the IPA that build kept', async () => {
  const { execFileSync } = require('node:child_process');
  // A real CMS-wrapped profile (self-signed), so the service's own decoding reads it.
  const signing = path.join(dir, 'signing', 'profiles'); fs.mkdirSync(signing, { recursive: true });
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'resign-chain-'));
  fs.writeFileSync(path.join(work, 'p.plist'), `<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>Name</key><string>Other</string><key>ExpirationDate</key><date>2099-01-01T00:00:00Z</date>
<key>Platform</key><array><string>xrOS</string></array><key>TeamIdentifier</key><array><string>T1</string></array>
<key>Entitlements</key><dict><key>application-identifier</key><string>T1.com.example.Other</string><key>get-task-allow</key><true/></dict>
</dict></plist>`);
  execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', path.join(work, 'k.pem'), '-out', path.join(work, 'c.pem'), '-subj', '/CN=fixture', '-days', '2'], { stdio: 'ignore' });
  execFileSync('openssl', ['smime', '-sign', '-in', path.join(work, 'p.plist'), '-signer', path.join(work, 'c.pem'), '-inkey', path.join(work, 'k.pem'),
    '-outform', 'DER', '-nodetach', '-binary', '-out', path.join(signing, 'other-dev.mobileprovision')]);
  // A stand-in build-and-sign: a --no-deploy build keeps an IPA in the store; --ipa "installs" it.
  const calls = path.join(work, 'calls.log'), fake = path.join(work, 'build-and-sign');
  fs.writeFileSync(fake, `#!/bin/bash
echo "$*" >> ${JSON.stringify(calls)}
if [[ " $* " == *" --no-deploy "* ]]; then
  id=fixture-chain; d="$APPARITION_BUILD_DIR/$id"; mkdir -p "$d"; printf 'ipa' > "$d/app.ipa"
  sha=$(shasum -a 256 "$d/app.ipa" | cut -d' ' -f1); mv "$d/app.ipa" "$d/$sha.ipa"
  printf '{"format":"apparition-build/1","buildId":"%s","platform":"visionOS","app":{"bundleId":"com.example.chain"},"artifact":{"kind":"ipa","sha256":"%s","bytes":3,"file":"%s.ipa"}}' "$id" "$sha" "$sha" > "$d/manifest.json"
  echo "Build ID: $id"
else
  echo "Build ID: fixture-resigned"   # build-and-sign --ipa keeps the re-signed IPA as a build of its own
fi
exit 0
`, { mode: 0o755 });
  const project = path.join(dir, 'Chain'); fs.mkdirSync(project);
  execFileSync('git', ['init', '-b', 'main', project], { stdio: 'ignore' });
  execFileSync('git', ['-C', project, '-c', 'user.name=t', '-c', 'user.email=t@example.com', 'commit', '--allow-empty', '-m', 'init'], { stdio: 'ignore' });
  const configFile = path.join(dir, 'config.json');
  const config = JSON.parse(fs.readFileSync(configFile));
  config.buildAndSign = fake;
  config.repos.push({ name: 'Chain', path: project, branches: [], platforms: [{ platform: 'visionOS', device: 'fixture-device', bundleId: 'com.example.chain' }] });
  fs.writeFileSync(configFile + '.next', JSON.stringify(config)); fs.renameSync(configFile + '.next', configFile);
  const token = await connect();
  for (let attempt = 0; attempt < 30; ++attempt) {
    if ((await request('/api/service/v1/state', { token })).body.repos.some(r => r.name === 'Chain')) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  const reply = await request('/api/service/v1/resign', { method: 'POST', token,
    body: { repo: 'Chain', device: 'fixture-device', profile: 'other-dev', dataMode: 'keep' } });
  assert.equal(reply.status, 200, JSON.stringify(reply.body));
  const [build, resign] = reply.body.jobs;
  assert.equal(build.forResign, true); assert.equal(build.noDeploy, true);
  assert.equal(resign.resign.after, build.id); assert.equal(resign.batchId, build.id);
  let done;
  for (let attempt = 0; attempt < 100 && !done; ++attempt) {
    const jobs = (await request('/api/service/v1/state', { token })).body.builds;
    done = jobs.find(job => job.id === resign.id && ['success', 'failed'].includes(job.status));
    if (!done) await new Promise(resolve => setTimeout(resolve, 100));
  }
  assert.equal(done?.status, 'success', done?.error);
  assert.equal(done.resign.from, 'fixture-chain');
  const lines = fs.readFileSync(calls, 'utf8').trim().split('\n');
  assert.match(lines[0], /--no-deploy/);
  assert.match(lines[1], /--ipa .*fixture-chain\/[0-9a-f]{64}\.ipa --device fixture-device --no-mcp --profile .*other-dev\.mobileprovision/);
  const state = (await request('/api/service/v1/state', { token })).body;
  assert.equal(state.installs['fixture-device']['com.example.chain'].resigned, 'other-dev', 'only the re-sign, not the build, is recorded as installed');
});

test('native entry patching accepts only a configured target and valid file list', async () => {
  const config = { passwordHash: 'hash', repos: [{ name: 'Example', platforms: [
    { device: 'AVP', platform: 'visionOS', bundleId: 'com.example.lab' },
  ] }] };
  const calls = [];
  const service = createService({ auth: { check: () => true }, config: () => config,
    readBody: async req => req.body, json: (res, status, body) => Object.assign(res, { status, body }),
    inject: async options => { calls.push(options); return { content: [], isError: false }; } });
  const valid = { repo: 'Example', device: 'AVP', targetPlatform: 'visionOS', files: ['Entry.swift'] };
  const req = body => ({ method: 'POST', headers: { authorization: 'Bearer ' + 'a'.repeat(43) }, body });
  let res = {};
  await service(req(valid), res, new URL('http://test/api/service/v1/inject'));
  assert.equal(res.status, 200); assert.equal(calls[0].target.bundleId, 'com.example.lab');
  assert.deepEqual(calls[0].files, ['Entry.swift']);
  for (const bad of [{ device: 'other' }, { targetPlatform: 'iOS' }, { repo: 'other' }, { files: 'Entry.swift' }, { files: [null] }]) {
    res = {};
    await service(req({ ...valid, ...bad }), res, new URL('http://test/api/service/v1/inject'));
    assert.equal(res.status, 400); assert.ok(res.body.hint);
  }
  assert.equal(calls.length, 1);
});

test('native build options preserve a known backend and reject guesses', async () => {
  const config = { passwordHash: 'hash', repos: [{ name: 'Example', platforms: [{ device: 'AVP', platform: 'visionOS' }] }] };
  const service = createService({ auth: { check: () => true }, config: () => config,
    readBody: async req => req.body, json: (res, status, body) => Object.assign(res, { status, body }) });
  const valid = { repo: 'Example', device: 'AVP', targetPlatform: 'visionOS', inject: true, injectBackend: 'apparition' };
  const req = body => ({ method: 'POST', headers: { authorization: 'Bearer ' + 'a'.repeat(43) }, body });
  const routed = await service(req(valid), {}, new URL('http://test/api/service/v1/build'));
  assert.equal(routed.body.injectBackend, 'apparition');
  const res = {};
  await service(req({ ...valid, injectBackend: 'jit' }), res, new URL('http://test/api/service/v1/build'));
  assert.equal(res.status, 400);
});

test('the service advertises install modes and device-reported installs', async () => {
  const r = await request('/api/service');
  assert.ok(r.body.capabilities.includes('install-modes'));
  assert.ok(r.body.capabilities.includes('report-install'));
});

test('a build names how it reaches the device, and only a known mode is accepted', async () => {
  const config = { passwordHash: 'hash', repos: [{ name: 'Example', branches: ['main'], platforms: [
    { device: 'AVP', platform: 'visionOS' },
  ] }] };
  const service = createService({ auth: { check: () => true }, config: () => config,
    readBody: async req => req.body, json: (res, status, body) => Object.assign(res, { status, body }) });
  const req = body => ({ method: 'POST', headers: { authorization: 'Bearer ' + 'a'.repeat(43) }, body });
  const url = new URL('http://test/api/service/v1/build');
  const valid = { repo: 'Example', device: 'AVP', targetPlatform: 'visionOS', branch: 'main' };
  for (const install of ['auto', 'mac', 'device', 'none']) {
    const result = await service(req({ ...valid, install }), {}, url);
    assert.equal(result.body.install, install);
  }
  assert.equal((await service(req(valid), {}, url)).body.install, undefined);
  for (const install of ['ota', 7, null]) {
    const res = {};
    await service(req({ ...valid, install }), res, url);
    assert.equal(res.status, 400);
  }
});

test('a client that names its device on a request is open, until it goes quiet', async () => {
  const { createPresence } = require('../lib/install-mode');
  const presence = createPresence();
  const config = { passwordHash: 'hash', repos: [] };
  const service = createService({ auth: { check: () => true }, config: () => config, presence,
    readBody: async req => req.body, json: (res, status, body) => Object.assign(res, { status, body }) });
  const req = device => ({ method: 'GET', headers: { authorization: 'Bearer ' + 'a'.repeat(43),
    ...(device === undefined ? {} : { 'x-apparition-device': device }) } });
  const url = new URL('http://test/api/service/v1/state');
  await service(req(encodeURIComponent("Ixion's Vision Pro")), {}, url);
  assert.equal(presence.listening("Ixion's Vision Pro"), true);
  await service(req('%E0%A4%A'), {}, url);        // malformed escape: ignored, not an error
  await service(req(undefined), {}, url);
  assert.deepEqual(presence.open(), ["ixion's vision pro"]);
  // An unauthenticated request never marks anything open.
  const closed = createPresence();
  const denied = createService({ auth: { check: () => false }, config: () => ({ passwordHash: 'hash', repos: [] }), presence: closed,
    readBody: async req => req.body, json: (res, status, body) => Object.assign(res, { status, body }) });
  const res = {};
  await denied(req('AVP'), res, url);
  assert.equal(res.status, 401);
  assert.deepEqual(closed.open(), []);
});

test('an install report is recorded by the service from its own records, over the native session only', async () => {
  const body = { device: 'AVP', bundleId: 'com.example.nothing', buildId: 'fixture-build' };
  assert.equal((await request('/api/service/v1/installs', { method: 'POST', body })).status, 401);
  const token = await connect();
  const refused = await request('/api/service/v1/installs', { method: 'POST', token, body });
  assert.equal(refused.status, 400);       // not a configured target: nothing is written
  assert.match(refused.body.error, /not a configured target/);
  assert.equal((await request('/api/service/v1/installs', { method: 'POST', token, body: { nonsense: true } })).status, 400);
  const state = await request('/api/service/v1/state', { token });
  assert.equal(state.body.installs.AVP, undefined);   // the refused report wrote nothing
  assert.deepEqual(state.body.appsOpen, []);
});

test('install modes: only `mac` uses the Xcode tunnel, a failed tunnel install still leaves its build, and the device reports its own', async () => {
  const { execFileSync } = require('node:child_process');
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'install-modes-'));
  const calls = path.join(work, 'calls.log'), failure = path.join(work, 'fail-install'), fake = path.join(work, 'build-and-sign');
  // A stand-in build-and-sign: it keeps the build and says so before it installs, as the real one
  // does, and the install then fails while `fail-install` exists (a device out of the Mac's reach).
  fs.writeFileSync(fake, `#!/bin/bash
echo "$*" >> ${JSON.stringify(calls)}
id="modes-$(date +%s%N)"; d="$APPARITION_BUILD_DIR/$id"; mkdir -p "$d"; printf 'ipa' > "$d/app.ipa"
sha=$(shasum -a 256 "$d/app.ipa" | cut -d' ' -f1); mv "$d/app.ipa" "$d/$sha.ipa"
printf '{"format":"apparition-build/1","buildId":"%s","platform":"visionOS","app":{"bundleId":"com.example.modes","version":"1.0","build":"abc1234"},"artifact":{"kind":"ipa","sha256":"%s","bytes":3,"file":"%s.ipa"}}' "$id" "$sha" "$sha" > "$d/manifest.json"
printf '  %-12s %s\\n' "Build ID:" "$id"
if [[ " $* " != *" --no-deploy "* && -e ${JSON.stringify(failure)} ]]; then echo "ERROR: Device 'fixture-modes' not found. Is it connected and paired?" >&2; exit 1; fi
printf '  %-12s %s\\n' "Build ID:" "$id"
exit 0
`, { mode: 0o755 });
  const project = path.join(dir, 'Modes'); fs.mkdirSync(project);
  execFileSync('git', ['init', '-b', 'main', project], { stdio: 'ignore' });
  execFileSync('git', ['-C', project, '-c', 'user.name=t', '-c', 'user.email=t@example.com', 'commit', '--allow-empty', '-m', 'init'], { stdio: 'ignore' });
  const configFile = path.join(dir, 'config.json');
  const config = JSON.parse(fs.readFileSync(configFile));
  config.buildAndSign = fake;
  config.repos.push({ name: 'Modes', path: project, branches: [], platforms: [{ platform: 'visionOS', device: 'fixture-modes', bundleId: 'com.example.modes' }] });
  fs.writeFileSync(configFile + '.next', JSON.stringify(config)); fs.renameSync(configFile + '.next', configFile);
  const token = await connect();
  for (let attempt = 0; attempt < 30; ++attempt) {
    if ((await request('/api/service/v1/state', { token })).body.repos.some(r => r.name === 'Modes')) break;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  const build = async options => {
    const reply = await request('/api/service/v1/build', { method: 'POST', token,
      body: { repo: 'Modes', device: 'fixture-modes', targetPlatform: 'visionOS', branch: 'current', ...options } });
    assert.equal(reply.status, 200, JSON.stringify(reply.body));
    const id = reply.body.jobs[0].id;
    for (let attempt = 0; attempt < 150; ++attempt) {
      const job = (await request('/api/service/v1/state', { token })).body.builds.find(j => j.id === id);
      if (job && ['success', 'failed'].includes(job.status)) return job;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.fail('the build did not finish');
  };
  const ledger = async () => (await request('/api/service/v1/state', { token })).body.installs['fixture-modes']?.['com.example.modes'];
  const lastCall = () => fs.readFileSync(calls, 'utf8').trim().split('\n').pop();

  // device: built and kept, never sent through the tunnel, and nothing claimed about the device.
  const device = await build({ install: 'device', run: true, mcp: true });
  assert.equal(device.status, 'success', device.error);
  assert.equal(device.installVia, 'device'); assert.equal(device.noDeploy, true);
  assert.equal(device.run, false); assert.equal(device.mcp, false, 'a launch needs the tunnel; the device launches its own copy');
  assert.match(lastCall(), /--no-deploy/);
  assert.equal(await ledger(), undefined);

  // The device installed it and says so: the service records it from its own records.
  const report = body => request('/api/service/v1/installs', { method: 'POST', token, body });
  const told = await report({ device: 'fixture-modes', bundleId: 'com.example.modes', buildId: device.buildId });
  assert.equal(told.status, 200, JSON.stringify(told.body));
  assert.equal(told.body.install.via, 'apparition'); assert.equal(told.body.install.confirmed, true);
  assert.equal(told.body.install.commit, device.commit); assert.equal(told.body.install.version, '1.0');
  assert.equal((await ledger()).buildId, device.buildId);
  assert.equal((await report({ device: 'fixture-modes', bundleId: 'com.example.modes', buildId: 'no-such-build' })).status, 404);

  // mac: build-and-sign installs through devicectl itself. When that fails the build is still kept.
  fs.writeFileSync(failure, '');
  const failed = await build({ install: 'mac' });
  assert.equal(failed.status, 'failed'); assert.equal(failed.installVia, 'mac');
  assert.doesNotMatch(lastCall(), /--no-deploy/);
  assert.ok(failed.keptBuildId, 'the kept build is tied to the failed job');
  assert.equal(failed.keptBuildId, failed.buildId);
  assert.notEqual(failed.buildId, device.buildId);
  assert.equal((await ledger()).buildId, device.buildId, 'a failed install changes nothing about what is on the device');
  const artifacts = (await request('/api/service/v1/artifacts', { token })).body.builds;
  assert.ok(artifacts.some(a => a.buildId === failed.keptBuildId));
  fs.rmSync(failure);
  const installed = await build({ install: 'mac' });
  assert.equal(installed.status, 'success', installed.error);
  assert.equal((await ledger()).buildId, installed.buildId); assert.equal((await ledger()).confirmed, true);

  // The old request shape: deploy false builds only.
  const legacy = await build({ deploy: false });
  assert.equal(legacy.installVia, 'none'); assert.equal(legacy.noDeploy, true);

  // auto: the Xcode tunnel while that device's Apparition is closed, the device while it is open.
  assert.equal((await build({ install: 'auto' })).installVia, 'mac');
  const open = await request('/api/service/v1/state', { token, headers: { 'X-Apparition-Device': 'Fixture-Modes' } });
  assert.deepEqual(open.body.appsOpen, ['fixture-modes']);
  assert.deepEqual((await request('/api/service/v1/state', { token })).body.appsOpen, ['fixture-modes']);
  assert.equal((await build({ install: 'auto' })).installVia, 'device');

  // A target that isn't configured is refused rather than built as the first one.
  const refused = await request('/api/service/v1/build', { method: 'POST', token,
    body: { repo: 'Modes', device: 'someone-else', targetPlatform: 'visionOS', branch: 'current' } });
  assert.equal(refused.status, 400);
});
