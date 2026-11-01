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
const compiled = spawnSync('xcrun', ['swiftc', path.join(root, 'Probe/BuildService.swift'), path.join(root, 'Probe/InstallRoute.swift'),
  path.join(root, 'scripts/check-build-service.swift'), '-o', binary], { stdio: 'inherit' });
if (compiled.status !== 0) process.exit(1);
const token = 'A'.repeat(43); // Synthetic fixture value, never a real credential.
let job = null, resignJob = null, deviceJob = null, revoked = false, redirected = false, devicePolls = 0;
const reports = [];
const resigns = [];
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
      if (body.password === 'rejected-delayed') {
        await new Promise(resolve => setTimeout(resolve, 300));
        res.writeHead(401, { 'Content-Type': 'application/json' }); return res.end('{}');
      }
      if (body.password === 'rejected') { res.writeHead(401, { 'Content-Type': 'application/json' }); return res.end('{}'); }
      if (body.password === 'redirect') { res.writeHead(302, { Location: '/stolen' }); return res.end(); }
      if (body.password === 'oversized') {
        res.writeHead(200, { 'Content-Type': 'application/json', 'Content-Length': 9 * 1024 * 1024 });
        return res.end('{}');
      }
      assert.equal(body.password, 'fixture-password');
      successfulConnections++;
      return json({ protocol: 'apparition-service', version: 1, token, capabilities: ['builds', 'resign', 'resign-build', 'install-modes', 'report-install'] });
    }
    if (req.url === '/stolen') { redirected = true; return json({}); }
    assert.equal(req.headers.authorization, 'Bearer ' + token);
    // Every request names the device it comes from, percent-encoded, so the service knows Apparition is open.
    assert.equal(req.headers['x-apparition-device'], 'Fixture%20Vision%20Pro');
    assert.equal(revoked, false);
    if (req.url === '/api/service/v1/state') {
      if (deviceJob) {
        devicePolls++;
        if (devicePolls === 2) deviceJob.status = 'running';
        if (devicePolls >= 4) Object.assign(deviceJob, { status: 'success', buildId: 'device-build', commit: 'def5678' });
      }
      if (successfulConnections === 2 && !expiredOnce) {
        expiredOnce = true; res.writeHead(401, { 'Content-Type': 'application/json' }); return res.end('{}');
      }
      return json({ host: 'fixture', repos: [{ name: 'Fixture', branches: [], git: { branch: 'main', commit: 'abc1234', dirty: false },
        platforms: [{ platform: 'visionOS', device: 'AVP', bundleId: 'com.example.fixture' }, { platform: 'iOS', device: 'Phone', bundleId: 'com.example.fixture' }] }],
        devices: { fetchedAt: 1, list: [{ name: 'AVP', model: 'Apple Vision Pro', state: 'connected', udid: 'U1', platform: 'visionOS' }] },
        installs: { AVP: { 'com.example.fixture': { repo: 'Fixture', branch: 'main', commit: 'abc1234', buildId: 'fixture-build', buildNo: null, confirmed: true, at: 1 } } },
        appsOpen: ['fixture vision pro'],
        builds: [job, resignJob, deviceJob].filter(Boolean) });
    }
    if (req.url === '/api/service/v1/artifacts') return json({ builds: [{ buildId: 'fixture-build', platform: 'visionOS', project: 'Fixture',
      createdAt: '2026-10-31T00:00:00Z', app: { name: 'Fixture', bundleId: 'com.example.fixture', build: 'abc1234' } },
      ...(deviceJob?.buildId ? [{ buildId: 'device-build', platform: 'visionOS', project: 'Fixture', createdAt: '2026-11-01T00:00:00Z',
        app: { name: 'Fixture', bundleId: 'com.example.fixture', build: 'def5678' } }] : [])] });
    if (req.url === '/api/service/v1/traces') return json({ traces: [] });
    if (req.url === '/api/service/v1/crashlogs') return json({ crashLogs: [] });
    if (req.url === '/api/service/v1/resign/options') return json({ profiles: [
      { id: 'fixture-old', name: 'Old', appId: 'com.example.*', expired: true, debuggable: true },
      { id: 'fixture-dev', name: 'Fixture', appId: 'com.example.Other', expired: false, debuggable: true } ] });
    if (req.url === '/api/service/v1/resign') {
      resigns.push(body);
      if (body.profile === 'refused') { res.writeHead(400, { 'Content-Type': 'application/json' }); return res.end(JSON.stringify({ error: 'No configured project installs com.example.fixture on AVP.' })); }
      if (body.repo) return json({ jobs: [{ id: 3, repo: 'Fixture', platform: 'iOS', device: 'Phone', status: 'queued', noDeploy: true, forResign: true },
        { id: 4, repo: 'Fixture', platform: 'iOS', device: 'Phone', status: 'queued', resign: { after: 3, dryRun: false } }] });
      resignJob = { id: 2, repo: 'Fixture', platform: 'visionOS', device: 'AVP', status: 'queued', resign: { dryRun: true, after: null } };
      return json({ jobs: [resignJob] });
    }
    if (req.url === '/api/service/v1/build') {
      if (body.install === 'device') {
        // The device's own install: built and kept, the Mac never touches the device, nothing launched.
        assert.deepEqual(body, { repo: 'Fixture', device: 'AVP', targetPlatform: 'visionOS', branch: 'current',
          install: 'device', deploy: true, run: false, mcp: true, inject: true, injectBackend: 'dynamic', buildDebug: false });
        deviceJob = { id: 5, repo: 'Fixture', platform: 'visionOS', device: 'AVP', status: 'queued', noDeploy: true, installVia: 'device' };
        return json({ jobs: [deviceJob] });
      }
      assert.deepEqual(body, { repo: 'Fixture', device: 'AVP', targetPlatform: 'visionOS', branch: 'current',
        install: 'none', deploy: false, run: false, mcp: true, inject: false, injectBackend: 'dynamic', buildDebug: false });
      job = { id: 1, repo: 'Fixture', platform: 'visionOS', device: 'AVP', status: 'queued', installVia: 'none' };
      return json({ jobs: [job] });
    }
    if (req.url === '/api/service/v1/installs') {
      reports.push(body);
      return json({ ok: true, install: { buildId: body.buildId, confirmed: true, via: 'apparition' } });
    }
    if (req.url === '/api/service/v1/builds/1/log') return json({ chunk: 'fixture build log' });
    if (req.url === '/api/service/v1/builds/1/cancel') { job.status = 'cancelled'; return json({ ok: true }); }
    if (req.url === '/api/service/v1/disconnect') { revoked = true; return json({ ok: true }); }
    throw new Error('Unexpected fixture route ' + req.url);
  } catch (error) { console.error(error); process.exitCode = 1; res.writeHead(500); res.end(); }
});
server.listen(0, '127.0.0.1', async () => {
  const child = spawn(binary, [`http://127.0.0.1:${server.address().port}`], { stdio: 'inherit' });
  const timer = setTimeout(() => { child.kill('SIGTERM'); process.exitCode = 1; }, 30000);
  child.on('exit', code => {
    clearTimeout(timer);
    try { assert.equal(code, 0); assert.equal(revoked, true); assert.equal(redirected, false); assert.equal(expiredOnce, true); assert.equal(successfulConnections, 3); assert.ok(requests.length > 8);
      assert.deepEqual(reports, [{ device: 'AVP', bundleId: 'com.example.fixture', buildId: 'device-build', platform: 'visionOS' }]);
      assert.deepEqual(resigns, [{ buildId: 'fixture-build', device: 'AVP', profile: 'fixture-dev', dataMode: 'discard', dryRun: true },
        { buildId: 'fixture-build', device: 'AVP', profile: 'refused', dataMode: 'keep', dryRun: false },
        { repo: 'Fixture', platform: 'iOS', device: 'Phone', profile: 'fixture-dev', dataMode: 'keep', dryRun: false }]); }
    catch (error) { console.error(error); process.exitCode = 1; }
    server.close(); server.closeAllConnections();
  });
});
