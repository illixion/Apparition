'use strict';
// Live debugging of running dev builds, for the web UI.
//
// Apps are found by DebugTrace's `debugtrace-mcp discover` (Bonjour plus the
// `bas --mcp` session records), so there is one discovery implementation.
// Each app is addressed by its build's key id; its URL and bearer token never
// come from the browser. The URL comes from discovery and the token from the
// key ledger, so the page can't be used to reach any other host.
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { execFile } = require('node:child_process');

const DISCOVERY_TTL_MS = 5000;
// How long one call to an app may take. The first call from this page waits
// for the person at the device to answer "Allow debug access?" (DebugTrace
// gives up after 60 s), and `_profile` then samples for up to 10 s, so the
// proxy has to outlast both, with room for a slow tailnet hop.
const CALL_TIMEOUT_MS = 90000;
const CLIENT_NAME = `App Store page on ${require('node:os').hostname().split('.')[0]}`;

// `tool` is a path or a function returning one (it follows config reloads).
function createDebug({ tool, keyDir, panelsDir, log = () => {}, callTimeoutMs = CALL_TIMEOUT_MS }) {
  // Tuning panels an agent wrote with debugtrace-mcp's `panel` tool, per bundle id; beside the key ledger.
  const panelRoot = () => panelsDir || path.join(path.dirname(keyDir), 'panels');
  const toolPath = () => (typeof tool === 'function' ? tool() : tool);
  let cache = { at: 0, apps: [], error: null };
  let inflight = null;

  function discover() {
    if (Date.now() - cache.at < DISCOVERY_TTL_MS) return Promise.resolve(cache);
    if (inflight) return inflight;
    inflight = new Promise(resolve => {
      const tool = toolPath();
      if (!tool) {
        cache = { at: Date.now(), apps: [], error: 'set "debugtraceTool" in config.json to the path of DebugTrace/Tools/debugtrace-mcp' };
        return resolve(cache);
      }
      execFile(tool, ['discover'], { timeout: 10000, maxBuffer: 1 << 20 }, (err, stdout) => {
        let apps = [], error = null;
        if (err) error = `discovery failed: ${err.code === 'ENOENT' ? `${tool} not found` : err.message}`;
        else {
          try { apps = JSON.parse(stdout); } catch { error = 'discovery returned something other than JSON'; }
        }
        cache = { at: Date.now(), apps: Array.isArray(apps) ? apps : [], error };
        resolve(cache);
      });
    }).finally(() => { inflight = null; });
    return inflight;
  }

  function token(keyId) {
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(keyId || '')) return null;
    try { return JSON.parse(fs.readFileSync(path.join(keyDir, `${keyId}.json`), 'utf8')).commandToken || null; } catch { return null; }
  }

  // What the browser may see: never the token.
  function publicApp(a) {
    const usable = !!(a.keyId && a.url && token(a.keyId));
    return {
      id: a.keyId || null, appName: a.appName, bundleId: a.bundleId, build: a.build || null, device: a.device,
      source: a.source || 'session', consoleLog: a.consoleLog ? true : undefined,
      attachable: usable && a.attachable !== false,
      reason: usable ? undefined : (a.reason || (a.keyId ? 'no command token for this build in the key ledger' : 'launched before per-build keys; relaunch it with build-and-sign')),
    };
  }

  async function find(id) {
    const { apps } = await discover();
    const app = apps.find(a => a.keyId === id);
    if (!app) return { error: [404, 'that app is no longer running, or is not on this network'] };
    const t = token(app.keyId);
    if (!t) return { error: [403, 'no command token for this build in the key ledger'] };
    return { app, token: t };
  }

  // One request to the app. Resolves {status, body (parsed JSON or text)}.
  function request(app, t, method, pathname, body, timeoutMs = callTimeoutMs) {
    return new Promise((resolve, reject) => {
      const url = new URL(pathname, app.url);
      const data = body === undefined ? null : Buffer.from(JSON.stringify(body));
      const req = http.request(url, {
        method, timeout: timeoutMs,
        // Named in the app's "Allow debug access?" prompt.
        headers: { Authorization: `Bearer ${t}`, Accept: 'application/json', 'X-DebugTrace-Client': CLIENT_NAME,
          ...(data ? { 'Content-Type': 'application/json', 'Content-Length': data.length } : {}) },
      }, res => {
        const chunks = [];
        res.on('data', c => chunks.push(c));
        res.on('end', () => {
          const raw = Buffer.concat(chunks);
          const contentType = res.headers['content-type'] || '';
          let parsed = null;
          if (contentType.includes('json')) {
            try { parsed = JSON.parse(raw.toString('utf8')); } catch { /* malformed */ }
          }
          resolve({ status: res.statusCode, body: parsed, raw, contentType });
        });
      });
      req.on('timeout', () => req.destroy(Object.assign(new Error(`the app did not answer within ${Math.round(timeoutMs / 1000)} s`), { code: 'timeout' })));
      req.on('error', reject);
      if (data) req.write(data);
      req.end();
    });
  }

  const unreachable = (app, e) => ({
    status: e.code === 'timeout' ? 504 : 502,
    body: { ok: false, error: { code: e.code === 'timeout' ? 'timeout' : 'unreachable', message: `${app.appName} did not answer: ${e.message}`,
      hint: e.code === 'timeout'
        ? `if the app is showing "Allow debug access?" for "${CLIENT_NAME}", choose Allow and run it again; otherwise bring the app to the foreground`
        : 'iOS suspends apps in the background; bring it to the foreground' } },
  });

  async function call(id, endpoint, args) {
    const found = await find(id);
    if (found.error) return { status: found.error[0], body: { ok: false, error: { message: found.error[1] } } };
    try {
      return await request(found.app, found.token, 'POST', '/_call', { name: endpoint, arguments: args || {} });
    } catch (e) { return unreachable(found.app, e); }
  }

  async function help(id) {
    const found = await find(id);
    if (found.error) return { status: found.error[0], body: { ok: false, error: { message: found.error[1] } } };
    try { return await request(found.app, found.token, 'GET', '/'); } catch (e) { return unreachable(found.app, e); }
  }

  // Server-sent events: `backlog` once, then a `lines` event per batch, and
  // `status` when the app stops or starts answering. Long-polls the app's
  // `_logs afterSequence=` so a line shows up within a fraction of a second.
  async function streamLogs(id, req, res, { level = 'debug', contains, backlog = 300 } = {}) {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store', Connection: 'keep-alive',
      'X-Accel-Buffering': 'no' });
    let closed = false;
    req.on('close', () => { closed = true; });
    const send = (event, data) => { if (!closed) res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`); };
    const heartbeat = setInterval(() => { if (!closed) res.write(': keep-alive\n\n'); }, 15000);

    let cursor = 0, first = true, reachable = null;
    const filter = { level, ...(contains ? { contains } : {}), format: 'json' };
    try {
      while (!closed) {
        const found = await find(id);
        if (found.error) {
          // Suspended apps can drop their advertisement; keep looking, slowly.
          if (reachable !== false) send('status', { state: 'gone', message: found.error[1] });
          reachable = false;
          await new Promise(r => setTimeout(r, 3000));
          continue;
        }
        let reply;
        try {
          reply = await request(found.app, found.token, 'POST', '/_call', {
            name: '_logs',
            arguments: first ? { ...filter, afterSequence: 0, limit: backlog } : { ...filter, afterSequence: cursor, waitSeconds: 20, limit: 1000 },
          });
        } catch (e) {
          if (reachable !== false) send('status', { state: 'unreachable', message: e.message });
          reachable = false;
          await new Promise(r => setTimeout(r, 2000));
          continue;
        }
        if (reachable !== true) { send('status', { state: 'live' }); reachable = true; }
        const data = reply.body?.data;
        if (!reply.body?.ok || !data) {
          send('status', { state: 'error', message: reply.body?.error?.message || `HTTP ${reply.status}` });
          await new Promise(r => setTimeout(r, 2000));
          continue;
        }
        // A smaller cursor than ours means the app relaunched: start over.
        if (!first && data.lastSequence < cursor) { cursor = 0; first = true; send('status', { state: 'relaunched' }); continue; }
        cursor = data.lastSequence ?? cursor;
        if (first || data.entries.length) send(first ? 'backlog' : 'lines', { entries: data.entries, truncated: data.truncated });
        first = false;
      }
    } catch (e) {
      log(`log stream ${id} ended: ${e.message}`);
      send('status', { state: 'error', message: e.message });
    } finally {
      clearInterval(heartbeat);
      if (!closed) res.end();
    }
  }

  // The app's saved panels, newest first. Read from disk on every request so an agent's
  // edit shows on the page's next poll; malformed files are skipped.
  async function panels(id) {
    const found = await find(id);
    if (found.error) return { status: found.error[0], body: { ok: false, error: { message: found.error[1] } } };
    const bundle = found.app.bundleId;
    if (!/^[A-Za-z0-9.-]{1,200}$/.test(bundle || '')) return { status: 200, body: { ok: true, panels: [] } };
    const dir = path.join(panelRoot(), bundle);
    const list = [];
    let names = [];
    try { names = fs.readdirSync(dir).filter(n => /^[a-z0-9][a-z0-9-]{0,47}\.json$/.test(n)); } catch { /* none yet */ }
    for (const name of names) {
      try {
        const panel = JSON.parse(fs.readFileSync(path.join(dir, name), 'utf8'));
        if (panel && Array.isArray(panel.controls)) list.push({ ...panel, name: name.slice(0, -5) });
      } catch { /* skip */ }
    }
    list.sort((a, b) => String(b.updatedAt || '').localeCompare(String(a.updatedAt || '')));
    return { status: 200, body: { ok: true, bundleId: bundle, panels: list } };
  }

  return { discover, publicApp, call, help, streamLogs, find, panels, clientName: CLIENT_NAME };
}

module.exports = { createDebug, CALL_TIMEOUT_MS, CLIENT_NAME };
