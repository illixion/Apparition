#!/usr/bin/env node
'use strict';
// A throwaway store wired to a fake app, for working on the Debug view with
// no device and without touching the live service:
//
//   node test/fixtures/debug-sandbox.js [--port 8499] [--approval-ms 4000] [--approval timeout]
//
// It starts fake-debug-app.js, writes a temp key ledger, discovery tool and
// config (no password), and runs server.js against them on --port. Ctrl-C
// stops both.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { createFakeApp } = require('./fake-debug-app');

const arg = (name, def) => { const i = process.argv.indexOf(name); return i > 0 ? process.argv[i + 1] : def; };
const port = Number(arg('--port', 8499));
const approvalMs = Number(arg('--approval-ms', 0));
const approval = arg('--approval', 'allow');

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'store-sandbox-'));
const keys = path.join(tmp, 'keys');
fs.mkdirSync(keys);
const app = createFakeApp({ token: 'sandbox-token', approvalMs, approval });
app.listen(0, '127.0.0.1', () => {
  const appUrl = `http://127.0.0.1:${app.address().port}`;
  fs.writeFileSync(path.join(keys, 'kExample.json'), JSON.stringify({ keyId: 'kExample', commandToken: 'sandbox-token', bundleId: 'com.example.ExampleApp', build: 'be18d4a' }));
  // A panel as debugtrace-mcp's `panel` tool writes it, beside the key ledger.
  fs.mkdirSync(path.join(tmp, 'panels', 'com.example.ExampleApp'), { recursive: true });
  fs.writeFileSync(path.join(tmp, 'panels', 'com.example.ExampleApp', 'hdr-tuning.json'), JSON.stringify({
    name: 'hdr-tuning', title: 'HDR tuning', note: 'Watch the window highlights against the sky while moving peak nits.', updatedAt: new Date().toISOString(),
    controls: [
      { kind: 'slider', tunable: 'hdr.peakNits' },
      { kind: 'choice', tunable: 'hdr.toneMap', label: 'Curve' },
      { kind: 'choice', endpoint: 'setWeather', argument: 'kind', choices: ['clear', 'rain', 'snow'], label: 'Weather' },
      { kind: 'readout', endpoint: 'renderStats', path: 'fps', label: 'FPS', refreshSeconds: 1 },
      { kind: 'readout', endpoint: 'renderStats', path: 'frameMs.p99', label: 'Frame p99 (ms)' },
      { kind: 'slider', tunable: 'hdr.missingOne', label: 'Not read yet' },
      { kind: 'button', endpoint: 'renderStats', label: 'Sample stats' },
    ] }));
  const tool = path.join(tmp, 'discover');
  fs.writeFileSync(tool, `#!/bin/sh\ncat <<'JSON'\n${JSON.stringify([
    { appName: 'ExampleApp', bundleId: 'com.example.ExampleApp', build: 'be18d4a', device: 'avp', url: appUrl, keyId: 'kExample', source: 'session', attachable: true },
    { appName: 'ExampleViewer', bundleId: 'com.example.ExampleViewer', build: '3b82d66', device: 'iphone', url: 'http://127.0.0.1:9', source: 'bonjour', attachable: false, reason: 'launched before per-build keys; relaunch it with build-and-sign' },
  ])}\nJSON\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(tmp, 'config.json'), JSON.stringify({ port, bundlePrefix: 'com.example', repos: [], debugtraceTool: tool }));
  const server = spawn(process.execPath, [path.join(__dirname, '..', '..', 'server.js')], {
    env: { ...process.env, STORE_CONFIG: path.join(tmp, 'config.json'), STORE_DATA: path.join(tmp, 'data'), DEBUGTRACE_KEY_DIR: keys, PATH: '/usr/bin:/bin' },
    stdio: 'inherit',
  });
  console.log(`fake app on ${appUrl}; store on http://127.0.0.1:${port}/#debug (temp dir ${tmp})`);
  const stop = () => { server.kill(); app.close(); fs.rmSync(tmp, { recursive: true, force: true }); process.exit(0); };
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
  server.on('exit', code => { console.log(`store exited ${code}`); app.close(); process.exit(code ?? 1); });
});
