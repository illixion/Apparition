#!/usr/bin/env node
'use strict';
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync, spawn } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'apparition-device-service-'));
const binary = path.join(temp, 'check');
const compiled = spawnSync('xcrun', ['swiftc', path.join(root, 'Probe/BuildService.swift'),
  path.join(root, 'scripts/check-build-service.swift'), '-o', binary], { stdio: 'inherit' });
if (compiled.status !== 0) process.exit(1);
const token = 'A'.repeat(43); // Synthetic fixture value, never a real credential.
let job = null, revoked = false, redirected = false;
let successfulConnections = 0, expiredOnce = false;
const requests = [];
const server = http.createServer(async (req, res) => {
  try {
    let raw = ''; for await (const chunk of req) raw += chunk;
    const body = raw ? JSON.parse(raw) : null;
    requests.push(req.url);
    assert.equal(req.headers.cookie, undefined);
    const json = value => { res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(value)); };
    if (req.url === '/api/service/connect') {
      assert.equal(req.headers.authorization, undefined);
      if (body.password === 'rejected') { res.writeHead(401, { 'Content-Type': 'application/json' }); return res.end('{}'); }
      if (body.password === 'redirect') { res.writeHead(302, { Location: '/stolen' }); return res.end(); }
      if (body.password === 'oversized') {
        res.writeHead(200, { 'Content-Type': 'application/json', 'Content-Length': 9 * 1024 * 1024 });
        return res.end('{}');
      }
      assert.equal(body.password, 'fixture-password');
      successfulConnections++;
      return json({ protocol: 'apparition-service', version: 1, token });
    }
    if (req.url === '/stolen') { redirected = true; return json({}); }
    assert.equal(req.headers.authorization, 'Bearer ' + token);
    assert.equal(revoked, false);
    if (req.url === '/api/service/v1/state') {
      if (successfulConnections === 2 && !expiredOnce) {
        expiredOnce = true; res.writeHead(401, { 'Content-Type': 'application/json' }); return res.end('{}');
      }
      return json({ host: 'fixture', repos: [{ name: 'Fixture', branches: [],
        platforms: [{ platform: 'visionOS', device: 'AVP' }] }], builds: job ? [job] : [] });
    }
    if (req.url === '/api/service/v1/artifacts') return json({ builds: [] });
    if (req.url === '/api/service/v1/build') {
      assert.deepEqual(body, { repo: 'Fixture', device: 'AVP', targetPlatform: 'visionOS', branch: 'current',
        deploy: false, run: false, mcp: true, inject: false, buildDebug: false });
      job = { id: 1, repo: 'Fixture', platform: 'visionOS', device: 'AVP', status: 'queued' };
      return json({ jobs: [job] });
    }
    if (req.url === '/api/service/v1/builds/1/log') return json({ chunk: 'fixture build log' });
    if (req.url === '/api/service/v1/builds/1/cancel') { job.status = 'cancelled'; return json({ ok: true }); }
    if (req.url === '/api/service/v1/disconnect') { revoked = true; return json({ ok: true }); }
    throw new Error('Unexpected fixture route');
  } catch (error) { console.error(error); process.exitCode = 1; res.writeHead(500); res.end(); }
});
server.listen(0, '127.0.0.1', async () => {
  const child = spawn(binary, [`http://127.0.0.1:${server.address().port}`], { stdio: 'inherit' });
  const timer = setTimeout(() => { child.kill('SIGTERM'); process.exitCode = 1; }, 30000);
  child.on('exit', code => {
    clearTimeout(timer);
    try { assert.equal(code, 0); assert.equal(revoked, true); assert.equal(redirected, false); assert.equal(expiredOnce, true); assert.equal(successfulConnections, 3); assert.ok(requests.length > 8); }
    catch (error) { console.error(error); process.exitCode = 1; }
    server.close(); server.closeAllConnections();
  });
});
