'use strict';
// lib/debug.js against the fake app: the proxy must outlast the device's
// approval wait plus a long _profile, and say what to do when it doesn't.
const { test, describe, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createDebug, CALL_TIMEOUT_MS, CLIENT_NAME } = require('../lib/debug');
const { createFakeApp } = require('./fixtures/fake-debug-app');

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-debug-'));
let app;

function debugFor(opts = {}) {
  return createDebug({ tool: path.join(TMP, 'discover'), keyDir: TMP, ...opts });
}

before(async () => {
  app = createFakeApp({ token: 't0k', approvalMs: 300 });
  await new Promise(r => app.listen(0, '127.0.0.1', r));
  fs.writeFileSync(path.join(TMP, 'k1.json'), JSON.stringify({ keyId: 'k1', commandToken: 't0k' }));
  fs.writeFileSync(path.join(TMP, 'discover'), `#!/bin/sh\necho '${JSON.stringify([
    { appName: 'Fake', bundleId: 'b', url: `http://127.0.0.1:${app.address().port}`, keyId: 'k1' },
  ])}'\n`, { mode: 0o755 });
});
after(() => app?.close());

describe('call timeout', () => {
  test('outlasts the 60 s approval wait plus a 10 s profile', () => {
    assert.ok(CALL_TIMEOUT_MS >= 60000 + 10000 + 5000, `${CALL_TIMEOUT_MS} ms is too short`);
  });
  test('a call that waits for approval and then samples comes back whole', async () => {
    const r = await debugFor().call('k1', '_profile', { seconds: 0.2, threads: 'all', limit: 3 });
    assert.equal(r.status, 200);
    assert.equal(r.body.ok, true);
    assert.equal(r.body.data.threads[0].busyPercent, 100);
    assert.ok(r.body.data.threads.every(t => t.selfTop.length <= 3));
  });
  test('a timeout is a 504 whose hint points at the approval prompt', async () => {
    const r = await debugFor({ callTimeoutMs: 100 }).call('k1', '_profile', { seconds: 1 });
    assert.equal(r.status, 504);
    assert.equal(r.body.error.code, 'timeout');
    assert.match(r.body.error.hint, /Allow debug access\?/);
    assert.ok(r.body.error.hint.includes(CLIENT_NAME));
  });
  test('the memory report has the fields the page reads', async () => {
    const r = await debugFor().call('k1', '_memory', { regions: 4 });
    const d = r.body.data;
    assert.equal(d.largestRegionKinds.length, 4);
    for (const k of ['footprintBytes', 'availableBytes', 'residentBytes']) assert.equal(typeof d.process[k], 'number');
  });
});
