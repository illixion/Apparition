'use strict';
// Native clients get a separate bearer session and a deliberately bounded API.
// Password changes and logout revoke it through the existing hashed-session store.
const ROUTES = [
  ['GET', /^\/state$/],
  ['POST', /^\/build$/],
  ['GET', /^\/builds\/\d+\/log$/],
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

function capabilities() {
  return { protocol: 'apparition-service', version: 1, hostPlatform: process.platform,
    capabilities: ['projects', 'devices', 'builds', 'build-logs', 'cancel-builds',
      'traces', 'crash-logs', 'debug-endpoints'], authentication: 'password-session' };
}

function createService({ auth, config, json, readBody }) {
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
    if (!config().passwordHash || !auth.check(token, config().passwordHash)) {
      json(res, 401, { error: 'Connect to the build service again.' });
      return { handled: true };
    }
    const suffix = url.pathname.slice(PREFIX.length);
    if ((!suffix || suffix === '/capabilities') && req.method === 'GET') {
      json(res, 200, capabilities());
      return { handled: true };
    }
    if (suffix === '/disconnect' && req.method === 'POST') {
      auth.logout(token);
      json(res, 200, { ok: true });
      return { handled: true };
    }
    if (!ROUTES.some(([method, pattern]) => method === req.method && pattern.test(suffix))) {
      json(res, 404, { error: 'Unknown service operation.' });
      return { handled: true };
    }
    if (suffix === '/build') {
      const body = await readBody(req).catch(() => ({})) || {};
      const repo = config().repos.find(r => r.name === body.repo);
      const target = repo?.platforms?.find(p => p.device === body.device && p.platform === body.targetPlatform);
      const branch = body.branch || 'current';
      if (!target || (branch !== 'current' && !repo.branches?.includes(branch)) ||
          ['run', 'mcp', 'inject', 'buildDebug', 'deploy'].some(k => body[k] !== undefined && typeof body[k] !== 'boolean')) {
        json(res, 400, { error: 'Select a configured project, target and branch, with boolean build options.' });
        return { handled: true };
      }
      return { authorized: true, path: '/api/build', body: { repo: repo.name,
        targets: [{ device: target.device, platform: target.platform, branch }],
        ...Object.fromEntries(['run', 'mcp', 'inject', 'buildDebug', 'deploy'].filter(k => body[k] !== undefined).map(k => [k, body[k]])) } };
    }
    return { authorized: true, path: '/api' + suffix };
  };
}

module.exports = { createService, capabilities };
