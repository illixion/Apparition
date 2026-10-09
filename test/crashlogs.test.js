'use strict';
const { test, describe } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
require('./helpers'); // sets the env first
const { createCrashLogs, crashRefs } = require('../lib/crashlogs');

const T0 = Date.parse('2026-10-09T08:00:00Z');

function ips(over = {}, body = {}) {
  const header = { app_name: 'App', app_version: '1.0', build_version: 'abc1234', bundleID: 'com.example.app',
    bug_type: '309', os_version: 'xrOS 27.2', slice_uuid: 'aaaa', incident_id: 'AAAA0000-0000-0000-0000-000000000001', ...over };
  return JSON.stringify(header) + '\n' + JSON.stringify({ pid: 42, captureTime: '2026-10-09 10:48:06.8041 +0300',
    exception: { type: 'EXC_BREAKPOINT', signal: 'SIGTRAP' }, termination: { indicator: 'Trace/BPT trap: 5' }, ...body });
}

// A device with `files` = {name: {modified, text}}. Counts copies; `down` makes it unreachable.
function fakeDevice(files) {
  const device = {
    files, copies: [], down: false,
    async list() {
      if (device.down) throw new Error('device not reachable');
      return Object.entries(device.files).map(([name, f]) => ({ name, modified: f.modified, isDirectory: false }));
    },
    async copy(_d, name, dest) { device.copies.push(name); fs.writeFileSync(dest, device.files[name].text); },
  };
  return device;
}

function make(device, extra = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'crashlogs-'));
  let clock = T0 + 3600000;
  const store = createCrashLogs({
    dir, devices: device, prefixes: () => ['com.example'], symbolicate: async () => 'symbolicated\n',
    now: () => clock, ...extra,
  });
  return { dir, store, tick: ms => { clock += ms; } };
}

describe('crash log pull', () => {
  test('keeps an app crash, symbolicated, and ignores other bundles', async () => {
    const device = fakeDevice({
      'Retired/App-2026-10-09-104807.ips': { modified: T0, text: ips() },
      'Retired/other-2026-10-09-104808.ips': { modified: T0 + 1000, text: ips({ bundleID: 'com.apple.other', incident_id: 'X' }) },
      'notes.txt': { modified: T0, text: 'x' },
    });
    const { store, dir } = make(device);
    assert.deepEqual(await store.pull('AVP'), { copied: 2, kept: 1 });
    const [e] = store.list();
    assert.equal(e.id, 'aaaa0000-0000-0000-0000-000000000001');
    assert.equal(e.bundleId, 'com.example.app');
    assert.equal(e.build, 'abc1234');
    assert.equal(e.exception, 'EXC_BREAKPOINT SIGTRAP');
    assert.equal(e.pid, 42);
    assert.equal(e.capturedAt, Date.parse('2026-10-09T07:48:06.804Z') ); // +0300, to the millisecond
    assert.ok(fs.existsSync(path.join(dir, e.file)));
    assert.equal(fs.readFileSync(path.join(dir, e.symbolicated), 'utf8'), 'symbolicated\n');
  });

  test('copies what is new once, not again', async () => {
    const device = fakeDevice({ 'a.ips': { modified: T0, text: ips() } });
    const { store } = make(device);
    await store.pull('AVP');
    await store.pull('AVP', { force: true });
    assert.deepEqual(device.copies, ['a.ips']);
    device.files['b.ips'] = { modified: T0 + 5000, text: ips({ incident_id: 'BBBB0000-0000-0000-0000-000000000002' }) };
    assert.deepEqual(await store.pull('AVP', { force: true }), { copied: 1, kept: 1 });
    assert.equal(store.list().length, 2);
  });

  test('two logs with the same modification second are both seen', async () => {
    const device = fakeDevice({ 'a.ips': { modified: T0, text: ips() } });
    const { store } = make(device);
    await store.pull('AVP');
    device.files['b.ips'] = { modified: T0, text: ips({ incident_id: 'BBBB0000-0000-0000-0000-000000000002' }) };
    assert.deepEqual(await store.pull('AVP', { force: true }), { copied: 1, kept: 1 });
  });

  test('the first look at a device goes back only backfillDays', async () => {
    const device = fakeDevice({
      'old.ips': { modified: T0 - 5 * 86400000, text: ips({ incident_id: 'OLD' }) },
      'recent.ips': { modified: T0 - 86400000, text: ips({ incident_id: 'RECENT' }) },
    });
    const { store } = make(device);
    assert.deepEqual(await store.pull('AVP'), { copied: 1, kept: 1 });
  });

  test('skips logs older than the age limit', async () => {
    const old = T0 - 20 * 86400000;
    const device = fakeDevice({ 'old.ips': { modified: old, text: ips() } });
    const { store } = make(device);
    assert.deepEqual(await store.pull('AVP'), { copied: 0, kept: 0 });
    device.files['older.ips'] = { modified: T0 - 16 * 86400000, text: ips({ incident_id: 'OLDER' }) };
    store.status().AVP.since = T0 - 30 * 86400000; // a device last seen a month ago
    assert.deepEqual(await store.pull('AVP', { force: true }), { copied: 0, kept: 0 });
  });

  test('copies at most perPoll logs, oldest first, and carries on next time', async () => {
    const files = {};
    for (let i = 0; i < 5; i++) files[`${i}.ips`] = { modified: T0 + i * 1000, text: ips({ incident_id: `CCCC000${i}` }) };
    const device = fakeDevice(files);
    const { store } = make(device, { perPoll: 2 });
    await store.pull('AVP');
    assert.deepEqual(device.copies, ['0.ips', '1.ips']);
    await store.pull('AVP', { force: true });
    assert.deepEqual(device.copies, ['0.ips', '1.ips', '2.ips', '3.ips']);
  });

  test('an unreachable device backs off, reports why, and recovers', async () => {
    const device = fakeDevice({ 'a.ips': { modified: T0, text: ips() } });
    const { store, tick } = make(device);
    device.down = true;
    assert.match((await store.pull('ixPhone')).error, /not reachable/);
    assert.match(store.status().ixPhone.error, /not reachable/);
    device.down = false;
    assert.equal(await store.pull('ixPhone'), null);          // still backing off
    tick(5 * 60000);
    assert.deepEqual(await store.pull('ixPhone'), { copied: 1, kept: 1 });
    assert.equal(store.status().ixPhone.error, null);
  });

  test('a failed copy does not move the high-water mark past it', async () => {
    const device = fakeDevice({ 'a.ips': { modified: T0, text: ips() } });
    const { store } = make(device);
    const copy = device.copy;
    device.copy = async () => { throw new Error('device locked'); };
    assert.match((await store.pull('AVP')).error, /locked/);
    device.copy = copy;
    assert.deepEqual(await store.pull('AVP', { force: true }), { copied: 1, kept: 1 });
  });

  test('trashing the files is forgetting the crash', async () => {
    const device = fakeDevice({
      'a.ips': { modified: T0, text: ips() },
      'b.ips': { modified: T0 + 1000, text: ips({ incident_id: 'BBBB0000' }) },
    });
    const { store, dir } = make(device);
    await store.pull('AVP');
    const [first] = store.list();
    fs.rmSync(path.join(dir, first.file));   // what `trash` does to it
    assert.equal(store.list().length, 1);
    assert.equal(store.get(first.id), null);
    assert.equal(fs.existsSync(path.join(dir, first.symbolicated)), false);
    await store.pull('AVP', { force: true });  // and it does not come back
    assert.equal(store.list().length, 1);
  });

  test('the index survives a restart', async () => {
    const device = fakeDevice({ 'a.ips': { modified: T0, text: ips() } });
    const { store, dir } = make(device);
    await store.pull('AVP');
    const again = createCrashLogs({ dir, devices: device, prefixes: () => ['com.example'], symbolicate: async () => null, now: () => T0 + 7200000 });
    assert.equal(again.list().length, 1);
    await again.pull('AVP', { force: true });
    assert.equal(device.copies.length, 1);
  });
});

describe('linking to MetricKit traces', () => {
  function traceWith(root, id, name, report) {
    fs.mkdirSync(path.join(root, id, 'files', 'crashes'), { recursive: true });
    fs.writeFileSync(path.join(root, id, 'files', 'crashes', name), JSON.stringify({ report }));
    return { id, app: { bundleId: 'com.example.app' } };
  }

  test('matches by app, process id and time, in both report layouts', async () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'traces-'));
    const metas = [
      traceWith(root, 't-new', 'a.json', { environment: { pid: 42, bundleIdentifier: 'com.example.app' },
        timeRange: { begin: '2026-10-09T07:48:06.804Z' }, crashDiagnostic: {} }),
      traceWith(root, 't-old', 'b.json', { timeStampBegin: '2026-10-09 10:48:06',
        crashDiagnostics: [{ diagnosticMetaData: { pid: 43, bundleIdentifier: 'com.example.app' } }] }),
      traceWith(root, 't-other', 'c.json', { environment: { pid: 99 }, timeRange: { begin: '2026-10-09T07:48:06.804Z' }, crashDiagnostic: {} }),
    ];
    const device = fakeDevice({
      'a.ips': { modified: T0, text: ips() },
      'b.ips': { modified: T0, text: ips({ incident_id: 'BBBB0000' }, { pid: 43 }) },
      'c.ips': { modified: T0, text: ips({ incident_id: 'CCCC0000' }, { pid: 100 }) },
    });
    const { store } = make(device);
    await store.pull('AVP');
    assert.equal(store.link(crashRefs(root, metas)), 2);
    const by = Object.fromEntries(store.list().map(e => [e.pid, e.traceId]));
    assert.deepEqual(by, { 42: 't-new', 43: 't-old', 100: null });
    assert.equal(store.link(crashRefs(root, metas)), 0); // already linked
  });

  test('a pid match far from the moment is not the same crash', async () => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'traces-'));
    const metas = [traceWith(root, 't', 'a.json', { environment: { pid: 42, bundleIdentifier: 'com.example.app' },
      timeRange: { begin: '2026-10-08T07:48:06.804Z' }, crashDiagnostic: {} })];
    const { store } = make(fakeDevice({ 'a.ips': { modified: T0, text: ips() } }));
    await store.pull('AVP');
    assert.equal(store.link(crashRefs(root, metas)), 0);
  });
});
