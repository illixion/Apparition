'use strict';
// Shared fixtures: a throwaway config/data/key ledger, and a DebugTrace zip writer.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const zlib = require('node:zlib');
const crypto = require('node:crypto');

const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'appstore-test-'));
const KEY_DIR = path.join(TMP, 'keys');
fs.mkdirSync(KEY_DIR);
fs.writeFileSync(path.join(TMP, 'config.json'), JSON.stringify({
  port: 0, bundlePrefix: 'com.example', repos: [],
}));
process.env.APPSTORE_CONFIG = path.join(TMP, 'config.json');
process.env.APPSTORE_DATA = path.join(TMP, 'data');
process.env.DEBUGTRACE_KEY_DIR = KEY_DIR;

const crcTable = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

// entries: [{name, data: Buffer, method?: 0|8, usize?, flags?}]. `usize` lets a
// test lie about the uncompressed size.
function makeZip(entries) {
  const locals = [], central = [];
  let offset = 0;
  for (const e of entries) {
    const name = Buffer.from(e.name);
    const method = e.method ?? 0;
    const raw = method === 8 ? zlib.deflateRawSync(e.data) : e.data;
    const usize = e.usize ?? e.data.length;
    const lh = Buffer.alloc(30);
    lh.writeUInt32LE(0x04034b50, 0); lh.writeUInt16LE(20, 4); lh.writeUInt16LE(e.flags ?? 0, 6);
    lh.writeUInt16LE(method, 8); lh.writeUInt32LE(crc32(e.data), 14);
    lh.writeUInt32LE(raw.length, 18); lh.writeUInt32LE(usize, 22); lh.writeUInt16LE(name.length, 26);
    const ch = Buffer.alloc(46);
    ch.writeUInt32LE(0x02014b50, 0); ch.writeUInt16LE(20, 4); ch.writeUInt16LE(20, 6);
    ch.writeUInt16LE(e.flags ?? 0, 8); ch.writeUInt16LE(method, 10); ch.writeUInt32LE(crc32(e.data), 16);
    ch.writeUInt32LE(raw.length, 20); ch.writeUInt32LE(e.zip64 ? 0xffffffff : usize, 24);
    ch.writeUInt16LE(name.length, 28); ch.writeUInt32LE(offset, 42);
    locals.push(lh, name, raw);
    central.push(ch, name);
    offset += 30 + name.length + raw.length;
  }
  const cd = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0); eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10); eocd.writeUInt32LE(cd.length, 12); eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cd, eocd]);
}

const sha256 = d => crypto.createHash('sha256').update(d).digest('hex');
const uuid = () => crypto.randomUUID();

// Registers a fresh signing key in the ledger, like build-and-sign does, and
// returns a builder for traces signed with it.
function newKey({ bundleId = 'com.example.app', build = 'abc1234' } = {}) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
  const raw = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32);
  const keyId = 'k' + crypto.randomBytes(6).toString('hex');
  fs.writeFileSync(path.join(KEY_DIR, `${keyId}.json`),
    JSON.stringify({ keyId, publicKey: raw.toString('base64'), bundleId, build }));
  return { keyId, privateKey, bundleId, build };
}

// Returns {zip, manifest}. Options let a test break one thing at a time.
function makeTrace(key, o = {}) {
  const files = o.files ?? { 'logs.txt': Buffer.from('hello\n') };
  const manifest = {
    format: o.format ?? 'debugtrace/1',
    traceId: o.traceId ?? uuid(),
    app: { bundleId: o.bundleId ?? key.bundleId, build: o.build ?? key.build },
    files: Object.entries(files).map(([p, d]) => ({
      path: p, bytes: d.length, sha256: o.badHash === p ? '0'.repeat(64) : sha256(d),
    })),
  };
  if (o.extraListed) manifest.files.push(o.extraListed);
  if (!o.unsigned) manifest.signature = { algorithm: o.algorithm ?? 'ed25519', keyId: o.keyId ?? key.keyId };
  const manifestBytes = Buffer.from(JSON.stringify(manifest));
  const sig = crypto.sign(null, o.signOther ?? manifestBytes, key.privateKey);
  const entries = [{ name: 'manifest.json', data: o.tamperManifest ? Buffer.from(JSON.stringify({ ...manifest, tampered: 1 })) : manifestBytes }];
  if (!o.unsigned) entries.push({ name: 'manifest.sig', data: sig });
  for (const [p, d] of Object.entries(files)) {
    if (o.omit === p) continue;
    if (o.inflateBomb === p) { entries.push({ name: p, data: Buffer.from('a'.repeat(100000)), method: 8, usize: d.length }); continue; }
    entries.push({ name: p, data: d, method: o.method });
  }
  for (const e of o.extraEntries ?? []) entries.push(e);
  return { zip: makeZip(entries), manifest };
}

module.exports = { TMP, KEY_DIR, makeZip, makeTrace, newKey, sha256 };
