#!/usr/bin/env node
// Real build-only integration check. Requires Xcode, the normal Apparition build
// prerequisites, and the appstore tools checkout (APPARITION_SERVICE_DIR overrides).
// The isolated service uses a random password and never installs or launches an app.
const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const root = path.resolve(__dirname, '..');
const service = process.env.APPARITION_SERVICE_DIR || path.resolve(root, '..', 'appstore');
const { hashPassword } = require(service + '/lib/auth');
fs.mkdirSync(path.join(root, 'build'), { recursive: true });
const directory = fs.mkdtempSync(path.join(root, 'build', 'service-artifact-e2e-'));
fs.chmodSync(directory, 0o700);

async function main() {
  const socket = net.createServer().listen(0, '127.0.0.1');
  await once(socket, 'listening'); const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  const password = crypto.randomBytes(32).toString('base64url');
  const config = { port, passwordHash: hashPassword(password), bundlePrefix: 'com.illixion',
    crashLogs: { enabled: false }, repos: [{ name: 'Apparition', path: root, branches: ['main'],
      platforms: [{ platform: 'visionOS', bundleId: 'com.illixion.apparition', device: 'AVP' }] }] };
  fs.writeFileSync(path.join(directory, 'config.json'), JSON.stringify(config), { mode: 0o600 });
  const log = fs.openSync(path.join(directory, 'server.log'), 'w', 0o600);
  const child = spawn(process.execPath, [service + '/server.js'], { cwd: service,
    env: { ...process.env, APPSTORE_CONFIG: path.join(directory, 'config.json'), APPSTORE_DATA: path.join(directory, 'data') },
    stdio: ['ignore', log, log] });
  const base = `http://127.0.0.1:${port}`;
  let token;
  let queuedId;
  async function request(route, body) {
    const response = await fetch(base + route, { method: body === undefined ? 'GET' : 'POST',
      headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body) });
    if (!response.ok) throw new Error('service operation failed with status ' + response.status);
    return response.json();
  }
  try {
    for (let i = 0; i < 50; ++i) {
      if (child.exitCode !== null || child.signalCode !== null) throw new Error('service exited before startup');
      try { await request('/api/service'); break; } catch { await new Promise(resolve => setTimeout(resolve, 100)); }
    }
    token = (await request('/api/service/connect', { password })).token;
    const queued = await request('/api/service/v1/build', { repo: 'Apparition', device: 'AVP', targetPlatform: 'visionOS',
      branch: 'current', deploy: false, run: false, mcp: false, inject: false, buildDebug: false });
    const id = queued.jobs[0].id;
    queuedId = id;
    console.log('Real authenticated build queued:', id);
    let job, previous;
    const deadline = Date.now() + 20 * 60 * 1000;
    while (Date.now() < deadline) {
      const state = await request('/api/service/v1/state');
      job = state.builds.find(build => build.id === id);
      if (job?.status !== previous) { console.log('Build state:', job?.status); previous = job?.status; }
      if (['success', 'failed', 'cancelled'].includes(job?.status)) break;
      await new Promise(resolve => setTimeout(resolve, 500));
    }
    if (job?.status !== 'success' || !job.buildId || !job.artifact || !job.source?.stable) {
      throw new Error('build did not produce a consistent immutable artifact; inspect the local server log');
    }
    const manifest = await request(`/api/service/v1/artifacts/${job.buildId}/manifest`);
    if (manifest.buildId !== job.buildId || manifest.artifact.sha256 !== job.artifact.sha256) throw new Error('job/manifest identity mismatch');
    const response = await fetch(base + `/api/service/v1/artifacts/${job.buildId}/download`, { headers: { Authorization: `Bearer ${token}` } });
    if (!response.ok) throw new Error('download failed');
    const hash = crypto.createHash('sha256'); let bytes = 0;
    for await (const chunk of response.body) { hash.update(chunk); bytes += chunk.length; }
    if (hash.digest('hex') !== manifest.artifact.sha256 || bytes !== manifest.artifact.bytes) throw new Error('download integrity failed');
    console.log(JSON.stringify({ verified: true, buildId: job.buildId, platform: job.platform,
      deploymentSkipped: job.noDeploy, sourceStable: job.source.stable, bytesVerified: bytes,
      repositories: manifest.source.before.sources.length, binaryUUIDs: Object.keys(manifest.binaries).length }));
    await request('/api/service/v1/disconnect', {}); token = undefined;
  } finally {
    // This is an isolated service and this job belongs to this check. On a
    // failure, ask it to stop its recorded build process before stopping it.
    if (token && queuedId) {
      try { await request(`/api/service/v1/builds/${queuedId}/cancel`, {}); } catch {}
    }
    if (child.exitCode === null && child.signalCode === null) { child.kill('SIGTERM'); await once(child, 'exit'); }
    fs.closeSync(log);
  }
}
main().catch(() => { console.error('E2E verification failed; inspect the local build/service-artifact-e2e-* logs.'); process.exitCode = 1; });
