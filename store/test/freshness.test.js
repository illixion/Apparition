'use strict';
const { test, describe } = require('node:test');
const assert = require('node:assert/strict');
const { platformStatus, agoFrom, esc } = require('../public/freshness');

const NOW = 1_000_000_000_000;
const P = { device: 'Phone', bundleId: 'com.example.app', platform: 'iOS' };
const HEAD = { commit: 'abc1234', branch: 'main', dirty: false };
const state = ({ buildNo, version = '1.0', inv, led, ota } = {}) => ({
  deviceApps: { Phone: inv === null ? undefined : (inv ?? { fetchedAt: NOW - 60_000, apps: buildNo === undefined ? {} : { [P.bundleId]: { version, buildNo } } }) },
  installs: led ? { Phone: { [P.bundleId]: led } } : {},
  ota: ota ?? {},
});
const status = (s, g = HEAD) => platformStatus(P, g, s, NOW);

describe('platformStatus', () => {
  test('on-device SHA equals HEAD: up to date', () => {
    const r = status(state({ buildNo: 'abc1234' }));
    assert.equal(r.dot, 'ok'); assert.equal(r.label, 'up to date');
  });
  test('a build number carrying a packages tag is judged by its app SHA', () => {
    assert.equal(status(state({ buildNo: 'abc1234-3fa1c2' })).label, 'up to date');
    assert.equal(status(state({ buildNo: 'abc1234-3fa1c2-dirty' })).label, 'dirty match');
    assert.equal(status(state({ buildNo: 'def5678-3fa1c2' })).label, 'out of date');
  });
  test('short SHA on the device matches a full-length HEAD', () => {
    const r = status(state({ buildNo: 'abc1234' }), { ...HEAD, commit: 'abc1234def5678abc1234def5678abc1234def5' });
    assert.equal(r.label, 'up to date');
  });
  test('different SHA: out of date, and says which HEAD', () => {
    const r = status(state({ buildNo: 'fffffff' }));
    assert.equal(r.dot, 'bad'); assert.equal(r.label, 'out of date');
    assert.match(r.detail, /abc1234/);
  });
  test('device build was dirty: dirty match, even when HEAD is clean', () => {
    const r = status(state({ buildNo: 'abc1234-dirty' }));
    assert.equal(r.dot, 'warn'); assert.equal(r.label, 'dirty match');
  });
  test('repo has uncommitted changes: dirty match', () => {
    const r = status(state({ buildNo: 'abc1234' }), { ...HEAD, dirty: true });
    assert.equal(r.label, 'dirty match');
  });
  test('"-dirty" is matched case-insensitively', () => {
    assert.equal(status(state({ buildNo: 'abc1234-DIRTY' })).label, 'dirty match');
  });
  test('built from another branch than the one shown', () => {
    const r = status(state({ buildNo: 'fffffff', led: { branch: 'feature', commit: 'fffffff', at: NOW } }));
    assert.equal(r.dot, 'info'); assert.equal(r.label, 'from feature');
  });
  test('same SHA beats a branch difference', () => {
    const r = status(state({ buildNo: 'abc1234', led: { branch: 'feature', commit: 'abc1234', at: NOW } }));
    assert.equal(r.label, 'up to date');
  });
  test('non-SHA build number cannot be compared and falls back to the ledger commit', () => {
    const r = status(state({ buildNo: '42', led: { branch: 'main', commit: 'abc1234', at: NOW } }));
    assert.equal(r.label, 'up to date');
  });
  test('non-SHA build number with no ledger shows only the version', () => {
    const r = status(state({ buildNo: '42' }));
    assert.equal(r.dot, 'info'); assert.equal(r.label, 'v1.0');
    assert.match(r.detail, /can't be checked/);
  });
  test('app absent from a fetched inventory: not installed', () => {
    assert.equal(status(state({})).label, 'not installed');
  });
  test('inventory not fetched yet: pending', () => {
    assert.equal(status(state({ inv: null })).label, 'pending…');
  });
  test('inventory error is surfaced', () => {
    const r = status(state({ inv: { error: 'device offline', erroredAt: NOW - 5000, apps: {} } }));
    assert.equal(r.dot, 'bad'); assert.equal(r.label, 'inventory error');
    assert.match(r.detail, /device offline/); assert.match(r.detail, /5s ago/);
  });
  test('ledger entry: confirmed LAN install vs unconfirmed OTA', () => {
    const lan = status(state({ buildNo: 'abc1234', led: { branch: 'main', commit: 'abc1234', at: NOW - 3_600_000 } }));
    assert.match(lan.detail, /LAN install of main@abc1234 confirmed 1h ago/);
    const ota = status(state({ buildNo: 'abc1234', led: { branch: 'main', commit: 'abc1234', at: NOW, confirmed: false } }));
    assert.match(ota.detail, /OTA build main@abc1234 offered .* not confirmed/);
  });
  test('returns the OTA record for the bundle and platform', () => {
    const ota = { 'com.example.app|iOS': { slug: 's' } };
    assert.deepEqual(status(state({ buildNo: 'abc1234', ota })).ota, { slug: 's' });
  });
  test('no ledger and no device record: "no build"-style fallback never throws', () => {
    assert.doesNotThrow(() => platformStatus(P, HEAD, { deviceApps: {}, installs: {} }, NOW));
  });
});

describe('agoFrom / esc', () => {
  test('time buckets', () => {
    assert.equal(agoFrom(NOW, 0), '—');
    assert.equal(agoFrom(NOW, NOW - 30_000), '30s ago');
    assert.equal(agoFrom(NOW, NOW - 5 * 60_000), '5m ago');
    assert.equal(agoFrom(NOW, NOW - 3 * 3_600_000), '3h ago');
    assert.equal(agoFrom(NOW, NOW - 2 * 86_400_000), '2d ago');
  });
  test('esc escapes HTML and tolerates null', () => {
    assert.equal(esc('<a href="x">&</a>'), '&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;');
    assert.equal(esc(null), '');
  });
});

describe('platformStatus on a ledger-only device', () => {
  const led = { branch: 'main', commit: 'abc1234', at: NOW - 3_600_000, confirmed: true };
  const st = (extra = {}) => ({ ...state({ inv: null, led }), ledgerOnly: ['Phone'], ...extra });
  test('no inventory: judged from the ledger instead of waiting on a fetch', () => {
    const r = status(st());
    assert.equal(r.label, 'up to date'); assert.match(r.detail, /not queried/);
  });
  test('ledger commit behind HEAD: out of date', () => {
    assert.equal(status(st(), { ...HEAD, commit: 'fffffff' }).label, 'out of date');
  });
  test('an inventory older than the last install is ignored', () => {
    const s = st({ deviceApps: { Phone: { fetchedAt: NOW - 86_400_000, apps: { [P.bundleId]: { version: '1.0', buildNo: 'fffffff' } } } } });
    assert.equal(status(s).label, 'up to date');
  });
  test('an inventory newer than the last install still wins', () => {
    const s = st({ deviceApps: { Phone: { fetchedAt: NOW - 60_000, apps: { [P.bundleId]: { version: '1.0', buildNo: 'fffffff' } } } } });
    assert.equal(status(s).label, 'out of date');
  });
  test('without ledger-only, an empty state still says pending', () => {
    assert.equal(status({ ...st(), ledgerOnly: [] }).label, 'pending…');
  });
});
