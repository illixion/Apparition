'use strict';
// Native clients get a separate bearer session and a deliberately bounded API.
// Password changes and logout revoke it through the existing hashed-session store.
const ROUTES = [
  ['GET', /^\/state$/],
  ['GET', /^\/artifacts$/],
  ['POST', /^\/build$/],
  ['POST', /^\/inject$/],
  ['POST', /^\/installs$/],
  ['POST', /^\/projects$/],
  ['GET', /^\/resign\/options$/],
  ['POST', /^\/resign$/],
  ['GET', /^\/builds\/\d+\/log$/],
  ['GET', /^\/artifacts\/[A-Za-z0-9-]{1,64}\/(manifest|download)$/],
  ['POST', /^\/builds\/\d+\/cancel$/],
  ['GET', /^\/traces$/],
  ['GET', /^\/traces\/[0-9a-f-]{36}\/files\/.+$/],
  ['GET', /^\/crashlogs$/],
  ['GET', /^\/crashlogs\/[0-9a-f-]{8,64}\/(ips|symbolicated)$/],
  ['GET', /^\/debug\/apps$/],
  ['GET', /^\/debug\/[A-Za-z0-9._-]{1,64}\/help$/],
  ['POST', /^\/debug\/[A-Za-z0-9._-]{1,64}\/call$/],
];
const PREFIX = '/api/service/v1';
const crypto = require('node:crypto');
const same = (a, b) => typeof a === 'string' && typeof b === 'string' && a.length === b.length
  && crypto.timingSafeEqual(Buffer.from(a), Buffer.from(b));

function capabilities() {
  return { protocol: 'apparition-service', version: 1, hostPlatform: process.platform,
    capabilities: ['projects', 'devices', 'builds', 'build-logs', 'cancel-builds',
      'traces', 'crash-logs', 'debug-endpoints', 'artifact-manifests', 'artifact-downloads', 'project-registration', 'resign', 'resign-build', 'entry-patching', 'install-modes', 'report-install'], authentication: 'password-session' };
}

// The name a client sends for the device it runs on. It is percent-encoded (headers are ASCII).
function deviceHeader(value) {
  if (typeof value !== 'string' || value.length > 400) return null;
  try { return decodeURIComponent(value); } catch { return null; }
}

// `hostToken()` is the token of the Apparition desktop app hosting this server (null when run
// directly). It is accepted as a bearer like a session, needs no password, and alone reaches
// `/host` (status) and `/host/web-ui` (turn the web pages on or off, set the password).
function createService({ auth, config, json, readBody, registerProject, inject, presence, reportInstall, hostToken = () => null, host = null }) {
  return async function route(req, res, url) {
    if (url.pathname === '/api/service' && req.method === 'GET') {
      json(res, 200, capabilities());
      return { handled: true };
    }
    if (url.pathname === '/api/service/connect' && req.method === 'POST') {
      if (!config().passwordHash) {
        json(res, 409, { error: 'Set a service password with node server.js set-password first.' });
      } else {
        const body = await readBody(req).catch(() => ({})) || {};
        const result = auth.login(body.password, config().passwordHash, { client: 'apparition' });
        if (!result.ok) json(res, result.status, { error: result.error });
        else json(res, 200, { ...capabilities(), token: result.token });
      }
      return { handled: true };
    }
    if (url.pathname !== PREFIX && !url.pathname.startsWith(PREFIX + '/')) return {};
    const match = /^Bearer ([A-Za-z0-9_-]{43})$/.exec(req.headers.authorization || '');
    const token = match?.[1];
    const isHost = same(token, hostToken());
    if (!isHost && (!config().passwordHash || !auth.check(token, config().passwordHash))) {
      json(res, 401, { error: 'Connect to the build service again.' });
      return { handled: true };
    }
    // Apparition names its device on every request while it is open, so a build asked for
    // `install: auto` goes to it and not through the Xcode tunnel; a closed one goes quiet.
    presence?.touch(deviceHeader(req.headers['x-apparition-device']));
    const suffix = url.pathname.slice(PREFIX.length);
    if ((!suffix || suffix === '/capabilities') && req.method === 'GET') {
      json(res, 200, capabilities());
      return { handled: true };
    }
    if (suffix === '/host' || suffix === '/host/web-ui' || suffix === '/host/web-link') {
      if (!isHost || !host) json(res, 404, { error: 'Unknown service operation.' });
      else if (suffix === '/host' && req.method === 'GET') json(res, 200, host.status());
      else if (suffix === '/host/web-link' && req.method === 'POST') json(res, 200, host.webLink());
      else if (suffix === '/host/web-ui' && req.method === 'POST') {
        try {
          host.setWebUi(await readBody(req).catch(() => ({})) || {});
          json(res, 200, host.status());
        } catch (error) {
          json(res, error.status || 500, { error: error.status ? error.message : 'Could not change the web UI setting.' });
        }
      } else json(res, 404, { error: 'Unknown service operation.' });
      return { handled: true };
    }
    if (suffix === '/disconnect' && req.method === 'POST') {
      if (isHost) { json(res, 200, { ok: true }); return { handled: true }; }
      auth.logout(token);
      json(res, 200, { ok: true });
      return { handled: true };
    }
    if (!ROUTES.some(([method, pattern]) => method === req.method && pattern.test(suffix))) {
      json(res, 404, { error: 'Unknown service operation.' });
      return { handled: true };
    }
    if (suffix === '/projects') {
      try {
        if (!registerProject) throw new Error('unavailable');
        const body = await readBody(req).catch(() => null);
        const project = registerProject(body);
        json(res, 200, { project });
      } catch (error) {
        const known = error instanceof require('./project-registration').RegistrationError;
        json(res, known ? error.status : 500, { error: known ? error.message : 'Could not register the project on the Mac.' });
      }
      return { handled: true };
    }
    if (suffix === '/installs') {
      try {
        if (!reportInstall) throw new Error('unavailable');
        const body = await readBody(req).catch(() => null);
        json(res, 200, { ok: true, install: reportInstall(body) });
      } catch (error) {
        const known = typeof error?.status === 'number';
        json(res, known ? error.status : 500, { ok: false, error: known ? error.message : 'Could not record the install.' });
      }
      return { handled: true };
    }
    if (suffix === '/inject') {
      const body = await readBody(req).catch(() => null);
      const repo = config().repos.find(r => r.name === body?.repo);
      const target = repo?.platforms?.find(p => p.device === body?.device && p.platform === body?.targetPlatform);
      if (!target?.bundleId || (body.files !== undefined && (!Array.isArray(body.files) || body.files.length > 64 || body.files.some(f => typeof f !== 'string' || f.length > 4096)))) {
        json(res, 400, { ok: false, error: 'Select a configured project and target, with an optional list of source paths.', hint: 'Use the same target as the running live-patching build.' });
      } else {
        try {
          if (!inject) throw new Error('Patching is unavailable on this service.');
          json(res, 200, await inject({ repo, target, files: body.files || [] }));
        } catch (error) {
          json(res, 409, { ok: false, error: error.message, hint: 'Launch a live-patching build and install the current Apparition CLI on the build Mac, then retry.' });
        }
      }
      return { handled: true };
    }
    if (suffix === '/build') {
      const body = await readBody(req).catch(() => ({})) || {};
      const repo = config().repos.find(r => r.name === body.repo);
      const target = repo?.platforms?.find(p => p.device === body.device && p.platform === body.targetPlatform);
      const branch = body.branch || 'current';
      if ((body.injectBackend !== undefined && !['dynamic', 'apparition'].includes(body.injectBackend)) || !target || (branch !== 'current' && !repo.branches?.includes(branch)) ||
          ['run', 'mcp', 'inject', 'buildDebug', 'deploy'].some(k => body[k] !== undefined && typeof body[k] !== 'boolean') ||
          (body.install !== undefined && !['auto', 'mac', 'device', 'none'].includes(body.install))) {
        json(res, 400, { error: 'Select a configured project, target and branch, with boolean build options and install auto, mac, device or none.' });
        return { handled: true };
      }
      return { authorized: true, path: '/api/build', body: { repo: repo.name,
        targets: [{ device: target.device, platform: target.platform, branch }],
        ...Object.fromEntries(['run', 'mcp', 'jit', 'inject', 'injectBackend', 'buildDebug', 'deploy', 'install'].filter(k => body[k] !== undefined).map(k => [k, body[k]])) } };
    }
    return { authorized: true, path: '/api' + suffix };
  };
}

module.exports = { createService, capabilities };
