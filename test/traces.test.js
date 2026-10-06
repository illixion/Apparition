'use strict';
const { test, describe } = require('node:test');
const assert = require('node:assert/strict');
const { makeZip, makeTrace, newKey } = require('./helpers'); // sets the env first
const { verifyTrace, safeTracePath } = require('../server');

const rejects = (buf, status, re) => assert.throws(() => verifyTrace(buf), e => {
  assert.equal(e.status, status, e.message);
  if (re) assert.match(e.message, re);
  return true;
});

describe('verifyTrace: accepts', () => {
  test('a signed trace with stored entries', () => {
    const key = newKey();
    const { zip, manifest } = makeTrace(key);
    const r = verifyTrace(zip);
    assert.equal(r.manifest.traceId, manifest.traceId);
    assert.equal(r.key.keyId, key.keyId);
    assert.deepEqual(r.files.map(f => f.path), ['logs.txt']);
    assert.equal(r.files[0].data.toString(), 'hello\n');
  });

  test('deflated entries', () => {
    const { zip } = makeTrace(newKey(), { method: 8, files: { 'logs.txt': Buffer.from('a'.repeat(5000)) } });
    assert.equal(verifyTrace(zip).files[0].data.length, 5000);
  });

  test('nested paths and several files', () => {
    const files = { 'a/b.txt': Buffer.from('1'), 'c.json': Buffer.from('{}') };
    assert.equal(verifyTrace(makeTrace(newKey(), { files }).zip).files.length, 2);
  });

  test('an empty file list', () => {
    assert.deepEqual(verifyTrace(makeTrace(newKey(), { files: {} }).zip).files, []);
  });
});

describe('verifyTrace: signature and ledger (403)', () => {
  test('unsigned trace, as from an Xcode build', () => {
    rejects(makeTrace(newKey(), { unsigned: true }).zip, 403, /unsigned/);
  });
  test('key that is not in the ledger', () => {
    rejects(makeTrace(newKey(), { keyId: 'nosuchkey' }).zip, 403, /not in this store's ledger/);
  });
  test('key id that tries to escape the ledger directory', () => {
    rejects(makeTrace(newKey(), { keyId: '../../etc/passwd' }).zip, 403, /not in this store's ledger/);
  });
  test('manifest edited after signing', () => {
    rejects(makeTrace(newKey(), { tamperManifest: true }).zip, 403, /does not verify/);
  });
  test('signature made over other bytes', () => {
    rejects(makeTrace(newKey(), { signOther: Buffer.from('other') }).zip, 403, /does not verify/);
  });
  test('signed by a different key than the one it names', () => {
    const a = newKey(), b = newKey();
    rejects(makeTrace(b, { keyId: a.keyId }).zip, 403, /does not verify/);
  });
  test('bundle id that differs from the key\'s', () => {
    rejects(makeTrace(newKey(), { bundleId: 'com.example.other' }).zip, 403, /belongs to/);
  });
  test('build that differs from the key\'s', () => {
    rejects(makeTrace(newKey(), { build: 'fffffff' }).zip, 403, /belongs to build/);
  });
  test('file whose content does not match its listed hash', () => {
    rejects(makeTrace(newKey(), { badHash: 'logs.txt' }).zip, 403, /manifest hash/);
  });
});

describe('verifyTrace: malformed archives (400)', () => {
  test('not a zip', () => rejects(Buffer.from('definitely not a zip'), 400, /end-of-central-directory/));
  test('empty buffer', () => rejects(Buffer.alloc(0), 400));
  test('zip without a manifest', () => rejects(makeZip([{ name: 'x', data: Buffer.from('x') }]), 400, /no manifest/));
  test('manifest that is not JSON', () => {
    rejects(makeZip([{ name: 'manifest.json', data: Buffer.from('nope') }]), 400, /not JSON/);
  });
  test('unknown format', () => rejects(makeTrace(newKey(), { format: 'debugtrace/9' }).zip, 400, /unknown trace format/));
  test('bad trace id', () => rejects(makeTrace(newKey(), { traceId: '../x' }).zip, 400, /traceId/));
  test('unsupported algorithm', () => rejects(makeTrace(newKey(), { algorithm: 'rsa' }).zip, 400, /unsupported signature/));
  test('file listed in the manifest but missing from the zip', () => {
    rejects(makeTrace(newKey(), { omit: 'logs.txt' }).zip, 400, /lacks it/);
  });
  test('file in the zip but not in the manifest', () => {
    rejects(makeTrace(newKey(), { extraEntries: [{ name: 'extra.txt', data: Buffer.from('x') }] }).zip, 400, /not in the manifest/);
  });
  test('duplicate entry names', () => {
    rejects(makeTrace(newKey(), { extraEntries: [{ name: 'logs.txt', data: Buffer.from('hello\n') }] }).zip, 400, /duplicate/);
  });
  test('path traversal in the zip', () => {
    rejects(makeTrace(newKey(), { extraEntries: [{ name: '../evil', data: Buffer.from('x') }] }).zip, 400, /unsafe path/);
  });
  test('manifest that lists a path outside the archive layout', () => {
    const files = { '../evil': Buffer.from('x') };
    rejects(makeTrace(newKey(), { files }).zip, 400, /unsafe path/);
  });
  test('entry that inflates beyond its declared size', () => {
    // Manifest and zip headers both say 6 bytes; the deflate stream holds 100000.
    rejects(makeTrace(newKey(), { inflateBomb: 'logs.txt' }).zip, 400, /does not inflate/);
  });
  test('encrypted entry', () => {
    rejects(makeZip([{ name: 'manifest.json', data: Buffer.from('{}'), flags: 1 }]), 400, /encrypted/);
  });
  test('zip64 archives', () => {
    rejects(makeZip([{ name: 'manifest.json', data: Buffer.from('{}'), zip64: true }]), 400, /zip64/);
  });
  test('unsupported compression method', () => {
    const zip = makeZip([{ name: 'manifest.json', data: Buffer.from('{}') }]);
    for (let i = 0; i < zip.length - 4; i++) if (zip.readUInt32LE(i) === 0x02014b50) zip.writeUInt16LE(12, i + 10);
    rejects(zip, 400, /compression method/);
  });
  test('central directory pointing outside the archive', () => {
    const zip = makeZip([{ name: 'manifest.json', data: Buffer.from('{}') }]);
    zip.writeUInt32LE(0x7fffffff, zip.length - 22 + 16);
    rejects(zip, 400);
  });
  test('truncated archive', () => {
    const { zip } = makeTrace(newKey());
    rejects(zip.subarray(0, zip.length - 30), 400);
  });
});

describe('safeTracePath', () => {
  for (const ok of ['logs.txt', 'a/b/c.json', 'dir/file.with.dots']) {
    test(`allows ${ok}`, () => assert.equal(safeTracePath(ok), true));
  }
  for (const bad of ['', '/abs', '../up', 'a/../b', './x', 'a//b', 'a\\b', 'a\0b', 'x'.repeat(256), null, 5, 'a/']) {
    test(`rejects ${JSON.stringify(bad)}`, () => assert.equal(safeTracePath(bad), false));
  }
});
