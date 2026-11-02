'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CLI = path.join(__dirname, '..', 'bin', 'crash-reports');

function data() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'crash-reports-'));
  fs.mkdirSync(path.join(dir, 'crashlogs', 'AVP'), { recursive: true });
  fs.writeFileSync(path.join(dir, 'crashlogs', 'AVP', 'App-1.ips'), 'x');
  fs.writeFileSync(path.join(dir, 'crashlogs', 'AVP', 'App-1.txt'), 'symbolicated stack\n');
  fs.writeFileSync(path.join(dir, 'crashlogs', 'index.json'), JSON.stringify({ entries: [{
    id: 'aaaa1111-0000', device: 'AVP', bundleId: 'com.example.app', appName: 'App', build: 'abc', exception: 'EXC_BREAKPOINT SIGTRAP',
    capturedAt: Date.parse('2026-10-09T08:00:00Z'), file: 'AVP/App-1.ips', symbolicated: 'AVP/App-1.txt', traceId: 'bbbb2222-0000' }] }));
  const t = path.join(dir, 'traces', 'bbbb2222-0000');
  fs.mkdirSync(path.join(t, 'files', 'crashes'), { recursive: true });
  fs.writeFileSync(path.join(t, 'meta.json'), JSON.stringify({ id: 'bbbb2222-0000', receivedAt: 1, app: { name: 'App', build: 'abc' },
    files: [{ path: 'crashes/r.json' }], build: { installedTo: 'AVP' } }));
  return dir;
}
const cli = (dir, ...args) => spawnSync(CLI, args, { encoding: 'utf8', env: { ...process.env, STORE_DATA: dir } });

test('list shows the crash log and the trace it links to', () => {
  const r = cli(data(), 'list');
  assert.equal(r.status, 0);
  assert.match(r.stdout, /aaaa1111 .*App .*EXC_BREAKPOINT SIGTRAP\s+trace bbbb2222/);
  assert.match(r.stdout, /bbbb2222 .*1 MetricKit report/);
});

test('list --app filters', () => {
  assert.match(cli(data(), 'list', '--app', 'nothing').stdout, /Crash logs from devices \(0\)/);
});

test('show prints the symbolicated text of a crash log', () => {
  assert.equal(cli(data(), 'show', 'aaaa').stdout, 'symbolicated stack\n');
});

test('done --dry-run names the crash log, its text and its trace, and moves nothing', () => {
  const dir = data();
  const r = cli(dir, 'done', 'aaaa1111', '--dry-run');
  assert.equal(r.status, 0);
  for (const f of ['crashlogs/AVP/App-1.ips', 'crashlogs/AVP/App-1.txt', 'traces/bbbb2222-0000']) assert.ok(r.stdout.includes(f), f);
  assert.ok(fs.existsSync(path.join(dir, 'crashlogs', 'AVP', 'App-1.ips')));
});

test('done on a trace also names the crash logs linked to it', () => {
  assert.match(cli(data(), 'done', 'bbbb2222', '--dry-run').stdout, /crashlogs\/AVP\/App-1\.ips/);
});

test('an unknown id is an error and nothing is touched', () => {
  const r = cli(data(), 'done', 'zzzz');
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no crash log or trace starts with zzzz/);
});
