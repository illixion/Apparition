'use strict';
// Crash logs pulled off the devices (`systemCrashLogs`), kept beside the traces.
//
// A MetricKit report (a trace's crashes/*.json) says what the OS told the app on its next launch;
// the .ips on the device is the full crash log, with Apple's frames already symbolicated, and it
// is the only report at all on tvOS (no MetricKit). The device I/O is injected, so tests need no
// device: `devices.list(device)` -> [{name, modified (ms), isDirectory}], `devices.copy(device,
// name, destination)`, and `symbolicate(ipsPath)` -> text.
//
// Only an app's crashes are kept: the header's bundleID must start with one of the tracked
// prefixes. Each device keeps a high-water mark (the modification time of the last log looked at),
// so a poll copies what is new and nothing twice. Logs Xcode has already "retired" sit under
// Retired/ and are listed too.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const KEEP = 300;

function parseIps(text) {
  const cut = text.indexOf('\n');
  if (cut < 0) return null;
  try { return { header: JSON.parse(text.slice(0, cut)), body: JSON.parse(text.slice(cut + 1)) }; } catch { return null; }
}

const slug = s => String(s).replace(/[^A-Za-z0-9._-]+/g, '_');

function createCrashLogs({ dir, devices, symbolicate, prefixes, maxAgeDays = 14, backfillDays = 3, perPoll = 100, now = Date.now, log = () => {} }) {
  fs.mkdirSync(dir, { recursive: true });
  const indexPath = path.join(dir, 'index.json');
  let index = { entries: [], pull: {} };
  try { index = { ...index, ...JSON.parse(fs.readFileSync(indexPath, 'utf8')) }; } catch { /* first run */ }
  const failures = new Map();   // device -> consecutive failures (memory only)
  const nextAt = new Map();     // device -> earliest next attempt, ms
  const inFlight = new Set();

  function save() {
    const tmp = indexPath + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(index, null, 1));
    fs.renameSync(tmp, indexPath);
  }

  function prune() {
    index.entries.sort((a, b) => b.capturedAt - a.capturedAt);
    for (const e of index.entries.splice(KEEP)) {
      fs.rmSync(path.join(dir, e.file), { force: true });
      if (e.symbolicated) fs.rmSync(path.join(dir, e.symbolicated), { force: true });
    }
  }

  // One device: list, copy what is new, keep the app's, symbolicate. Never throws.
  async function pull(device, { force = false } = {}) {
    if (inFlight.has(device)) return null;
    if (!force && (nextAt.get(device) || 0) > now()) return null;
    inFlight.add(device);
    const state = index.pull[device] = { since: 0, doneAtSince: [], ...index.pull[device] };
    try {
      const files = await devices.list(device);
      // The first look at a device goes back `backfillDays` only: a device keeps hundreds of
      // system logs, and what matters is the crashes from now on.
      const floor = state.since ? Math.max(state.since, now() - maxAgeDays * 86400000) : now() - backfillDays * 86400000;
      const fresh = files
        .filter(f => !f.isDirectory && f.name.endsWith('.ips') && f.modified >= floor
          && !(f.modified === state.since && state.doneAtSince.includes(f.name)))
        .sort((a, b) => a.modified - b.modified || a.name.localeCompare(b.name))
        .slice(0, perPoll);
      let kept = 0;
      for (const f of fresh) {
        const tmp = path.join(dir, `.incoming-${crypto.randomBytes(4).toString('hex')}.ips`);
        try {
          await devices.copy(device, f.name, tmp);
          const entry = await keep(device, f, tmp);
          if (entry) kept++;
        } finally {
          fs.rmSync(tmp, { force: true });
        }
        if (f.modified > state.since) { state.since = f.modified; state.doneAtSince = []; }
        state.doneAtSince.push(f.name);
      }
      Object.assign(state, { polledAt: now(), error: null, copied: fresh.length, kept });
      failures.delete(device);
      nextAt.delete(device);
      prune();
      return { copied: fresh.length, kept };
    } catch (e) {
      // A device that is away or asleep is the normal case: back off, up to half an hour.
      const n = (failures.get(device) || 0) + 1;
      failures.set(device, n);
      nextAt.set(device, now() + Math.min(30 * 60000, 60000 * 2 ** n));
      Object.assign(state, { polledAt: now(), error: String(e.message || e) });
      return { error: state.error };
    } finally {
      inFlight.delete(device);
      save();
    }
  }

  async function keep(device, f, tmp) {
    const text = fs.readFileSync(tmp, 'utf8');
    const ips = parseIps(text);
    const bundleId = ips?.header?.bundleID;
    if (!bundleId || !prefixes().some(p => bundleId.startsWith(p))) return null;
    const { header, body } = ips;
    const id = String(header.incident_id || crypto.createHash('sha256').update(text).digest('hex').slice(0, 32)).toLowerCase();
    if (index.entries.some(e => e.id === id)) return null;
    const rel = path.join(slug(device), `${slug(path.basename(f.name, '.ips'))}-${id.slice(0, 8)}.ips`);
    fs.mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
    fs.copyFileSync(tmp, path.join(dir, rel));
    const captured = Date.parse(String(body.captureTime || header.timestamp || '').replace(/ ([+-]\d\d)(\d\d)$/, '$1:$2').replace(' ', 'T')) || f.modified;
    const entry = {
      id, device, bundleId, appName: header.app_name || null, version: header.app_version || null,
      build: header.build_version || null, os: header.os_version || null, bugType: header.bug_type || null,
      exception: [body.exception?.type, body.exception?.signal].filter(Boolean).join(' ') || null,
      termination: body.termination?.indicator || null, pid: body.pid ?? null, sliceUuid: header.slice_uuid || null,
      capturedAt: captured, pulledAt: now(), file: rel, symbolicated: null, traceId: null,
    };
    try {
      const out = await symbolicate(path.join(dir, rel));
      if (out) {
        entry.symbolicated = rel.replace(/\.ips$/, '.txt');
        fs.writeFileSync(path.join(dir, entry.symbolicated), out);
      }
    } catch (e) { log(`symbolicate ${rel}: ${e.message || e}`); }
    index.entries.push(entry);
    log(`crash log ${rel} (${bundleId} ${entry.build}, ${entry.exception || 'no exception'})`);
    return entry;
  }

  // Ties each crash log to the MetricKit trace of the same crash: the same app, process id and,
  // within `tolerance`, the same moment. `refs` = [{traceId, bundleId, pid, at, tolerance}] from `crashRefs`.
  // Called again when a trace arrives, since either side can come first.
  function link(refs) {
    let n = 0;
    for (const e of index.entries) {
      if (e.traceId || e.pid == null) continue;
      const hit = refs.find(r => r.bundleId === e.bundleId && r.pid === e.pid && Math.abs(r.at - e.capturedAt) < r.tolerance);
      if (hit) { e.traceId = hit.traceId; n++; }
    }
    if (n) save();
    return n;
  }

  return {
    pull, link, parseIps,
    list: () => [...index.entries].sort((a, b) => b.capturedAt - a.capturedAt),
    get: id => index.entries.find(e => e.id === id) || null,
    status: () => index.pull,
    resolve: rel => path.join(dir, rel),
  };
}

// The MetricKit side of `link`: one ref per diagnostic in a trace's crashes/*.json, for either
// layout (27+: environment.pid and timeRange; before: diagnosticMetaData.pid and timeStampBegin).
function crashRefs(traceDir, metas) {
  const refs = [];
  for (const meta of metas) {
    const folder = path.join(traceDir, meta.id, 'files', 'crashes');
    let names = [];
    try { names = fs.readdirSync(folder); } catch { continue; }
    for (const name of names) {
      let report;
      try { report = JSON.parse(fs.readFileSync(path.join(folder, name), 'utf8')).report; } catch { continue; }
      if (!report) continue;
      // 27+ gives an exact UTC time. Before, a local time with no zone, whose device may not be
      // in the Mac's zone, so only the process id and app really identify the crash.
      const exact = Boolean(report.timeRange?.begin);
      const at = Date.parse(report.timeRange?.begin || report.timeStampBegin || '') || 0;
      const diagnostics = [report.crashDiagnostic, ...(report.crashDiagnostics || [])].filter(Boolean);
      for (const d of diagnostics) {
        const pid = report.environment?.pid ?? d.diagnosticMetaData?.pid;
        const bundleId = report.environment?.bundleIdentifier ?? d.diagnosticMetaData?.bundleIdentifier ?? meta.app?.bundleId;
        if (pid != null && at) refs.push({ traceId: meta.id, bundleId, pid, at, tolerance: exact ? 10 * 60000 : 48 * 3600000 });
      }
    }
  }
  return refs;
}

module.exports = { createCrashLogs, crashRefs, parseIps };
