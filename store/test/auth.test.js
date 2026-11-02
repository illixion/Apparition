'use strict';
const { test, describe } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createAuth, hashPassword, verifyPassword, parseCookies } = require('../lib/auth');

const dir = () => fs.mkdtempSync(path.join(os.tmpdir(), 'store-auth-'));
const HASH = hashPassword('correct horse');

describe('passwords', () => {
  test('a hash verifies only its own password', () => {
    assert.equal(verifyPassword('correct horse', HASH), true);
    assert.equal(verifyPassword('correct horsf', HASH), false);
    assert.equal(verifyPassword('', HASH), false);
  });
  test('each hash is salted', () => assert.notEqual(hashPassword('correct horse'), HASH));
  test('malformed or missing hashes never verify', () => {
    for (const bad of [undefined, null, '', 'plain', 'scrypt$1$2$3$x', HASH.replace('scrypt', 'bcrypt')]) {
      assert.equal(verifyPassword('correct horse', bad), false);
    }
  });
  test('short passwords are refused', () => assert.throws(() => hashPassword('short'), /8 characters/));
});

describe('sessions', () => {
  test('login, check, logout', () => {
    const auth = createAuth(dir());
    const r = auth.login('correct horse', HASH);
    assert.equal(r.ok, true);
    assert.equal(auth.check(r.token, HASH), true);
    assert.equal(auth.check('forged', HASH), false);
    auth.logout(r.token);
    assert.equal(auth.check(r.token, HASH), false);
  });
  test('a wrong password gets no token', () => {
    const r = createAuth(dir()).login('nope', HASH);
    assert.deepEqual([r.ok, r.status, r.token], [false, 401, undefined]);
  });
  test('changing the password signs every session out', () => {
    const auth = createAuth(dir());
    const { token } = auth.login('correct horse', HASH);
    assert.equal(auth.check(token, hashPassword('another password')), false);
  });
  test('sessions survive a restart, and only their hashes are stored', () => {
    const d = dir();
    const { token } = createAuth(d).login('correct horse', HASH);
    assert.equal(createAuth(d).check(token, HASH), true);
    const stored = fs.readFileSync(path.join(d, 'sessions.json'), 'utf8');
    assert.equal(stored.includes(token), false);
    assert.equal(fs.statSync(path.join(d, 'sessions.json')).mode & 0o777, 0o600);
    assert.equal(fs.statSync(path.join(d, 'secret')).mode & 0o777, 0o600);
  });
  test('sessions expire after 30 idle days', () => {
    let t = 1e12;
    const auth = createAuth(dir(), { now: () => t });
    const { token } = auth.login('correct horse', HASH);
    t += 29 * 86400e3;
    assert.equal(auth.check(token, HASH), true); // and this slides it
    t += 29 * 86400e3;
    assert.equal(auth.check(token, HASH), true);
    t += 31 * 86400e3;
    assert.equal(auth.check(token, HASH), false);
  });
  test('five wrong passwords lock logins for a minute, even the right one', () => {
    let t = 1e12;
    const auth = createAuth(dir(), { now: () => t });
    for (let i = 0; i < 5; i++) auth.login('wrong', HASH);
    const locked = auth.login('correct horse', HASH);
    assert.equal(locked.status, 429);
    t += 61e3;
    assert.equal(auth.login('correct horse', HASH).ok, true);
  });
  test('the cookie is HttpOnly and SameSite=Strict, and Secure over HTTPS', () => {
    const auth = createAuth(dir());
    assert.match(auth.cookie('x', false), /HttpOnly; SameSite=Strict/);
    assert.doesNotMatch(auth.cookie('x', false), /Secure/);
    assert.match(auth.cookie('x', true), /; Secure$/);
  });
  test('cookies parse', () => {
    assert.deepEqual(parseCookies('a=1; store_session=tok%3D; b'), { a: '1', store_session: 'tok=' });
  });
});

describe('signed OTA links', () => {
  test('valid for their path until they expire', () => {
    let t = 1e12;
    const auth = createAuth(dir(), { now: () => t });
    const sig = auth.signPath('/ota/app.ipa', 3600);
    assert.equal(auth.verifyPath('/ota/app.ipa', sig), true);
    assert.equal(auth.verifyPath('/ota/other.ipa', sig), false);
    t += 3601e3;
    assert.equal(auth.verifyPath('/ota/app.ipa', sig), false);
  });
  test('forged, malformed or another store\'s signatures fail', () => {
    const a = createAuth(dir()), b = createAuth(dir());
    const sig = a.signPath('/ota/app.ipa');
    assert.equal(b.verifyPath('/ota/app.ipa', sig), false);
    for (const bad of [null, '', 'x', `${sig}x`, sig.replace(/^\d+/, '99999999999')]) assert.equal(a.verifyPath('/ota/app.ipa', bad), false);
  });
});
