'use strict';
// Password login for the web UI and API, and signed links for the iOS
// installer, which can't send a cookie.
//
// - The password is stored as an scrypt hash (`passwordHash` in config.json,
//   written by `node server.js set-password`). With none set, everything is
//   open, as before: the server listens on 127.0.0.1 only.
// - A login sets an HttpOnly, SameSite=Strict session cookie. Only a SHA-256
//   of each session token is kept, in data/sessions.json (mode 600).
// - OTA manifest and IPA URLs carry `?t=<expiry>.<hmac>` over their path, so
//   installd can fetch them without a cookie and nobody can without the page.
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const COOKIE = 'appstore_session';
const SESSION_DAYS = 30;
const SCRYPT = { N: 1 << 15, r: 8, p: 1, maxmem: 64 * 1024 * 1024 };

function hashPassword(password) {
  if (typeof password !== 'string' || password.length < 8) throw new Error('use a password of at least 8 characters');
  const salt = crypto.randomBytes(16);
  const hash = crypto.scryptSync(password, salt, 32, SCRYPT);
  return `scrypt$${SCRYPT.N}$${SCRYPT.r}$${SCRYPT.p}$${salt.toString('base64')}$${hash.toString('base64')}`;
}

function verifyPassword(password, stored) {
  const parts = String(stored || '').split('$');
  if (parts.length !== 6 || parts[0] !== 'scrypt' || typeof password !== 'string') return false;
  const [, N, r, p, salt, hash] = parts;
  const expected = Buffer.from(hash, 'base64');
  let actual;
  try {
    actual = crypto.scryptSync(password, Buffer.from(salt, 'base64'), expected.length,
      { N: Number(N), r: Number(r), p: Number(p), maxmem: SCRYPT.maxmem });
  } catch { return false; }
  return expected.length > 0 && crypto.timingSafeEqual(actual, expected);
}

function parseCookies(header) {
  const out = {};
  for (const part of String(header || '').split(';')) {
    const i = part.indexOf('=');
    if (i > 0) out[part.slice(0, i).trim()] = decodeURIComponent(part.slice(i + 1).trim());
  }
  return out;
}

const sha = s => crypto.createHash('sha256').update(s).digest('hex');

// One per data directory: sessions, the link-signing secret, and the
// failed-login throttle.
function createAuth(dataDir, { now = () => Date.now() } = {}) {
  const sessionsFile = path.join(dataDir, 'sessions.json');
  const secretFile = path.join(dataDir, 'secret');
  fs.mkdirSync(dataDir, { recursive: true });

  let secret;
  try { secret = fs.readFileSync(secretFile); } catch { /* first run */ }
  if (!secret || secret.length < 32) {
    secret = crypto.randomBytes(32);
    fs.writeFileSync(secretFile, secret, { mode: 0o600 });
  }

  let sessions = {};
  try { sessions = JSON.parse(fs.readFileSync(sessionsFile, 'utf8')); } catch { /* none yet */ }
  const save = () => {
    const tmp = sessionsFile + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(sessions), { mode: 0o600 });
    fs.renameSync(tmp, sessionsFile);
  };
  const prune = () => {
    const cutoff = now() - SESSION_DAYS * 86400e3;
    for (const [k, s] of Object.entries(sessions)) if (s.lastSeen < cutoff) delete sessions[k];
  };

  // Five wrong passwords lock logins for a minute, for everyone: behind
  // tailscale serve every request comes from 127.0.0.1.
  const failures = [];
  let lockedUntil = 0;

  return {
    COOKIE,
    login(password, passwordHash, meta = {}) {
      const t = now();
      if (t < lockedUntil) return { ok: false, status: 429, error: `too many wrong passwords; try again in ${Math.ceil((lockedUntil - t) / 1000)} s` };
      if (!verifyPassword(password, passwordHash)) {
        failures.push(t);
        while (failures.length && failures[0] < t - 10 * 60e3) failures.shift();
        if (failures.length >= 5) { lockedUntil = t + 60e3; failures.length = 0; }
        return { ok: false, status: 401, error: 'wrong password' };
      }
      failures.length = 0;
      const token = crypto.randomBytes(32).toString('base64url');
      prune();
      // Tied to this password: changing it signs every session out.
      sessions[sha(token)] = { created: t, lastSeen: t, pw: sha(passwordHash).slice(0, 16), ...meta };
      save();
      return { ok: true, token };
    },
    // True for a live session made with the current password; slides its
    // expiry at most once an hour.
    check(token, passwordHash) {
      if (!token) return false;
      const s = sessions[sha(token)];
      if (!s || s.pw !== sha(String(passwordHash)).slice(0, 16)) return false;
      const t = now();
      if (s.lastSeen < t - SESSION_DAYS * 86400e3) { delete sessions[sha(token)]; save(); return false; }
      if (t - s.lastSeen > 3600e3) { s.lastSeen = t; save(); }
      return true;
    },
    logout(token) {
      if (token && sessions[sha(token)]) { delete sessions[sha(token)]; save(); }
    },
    logoutEverywhere() { sessions = {}; save(); },
    cookie(token, secure) {
      return `${COOKIE}=${token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=${SESSION_DAYS * 86400}${secure ? '; Secure' : ''}`;
    },
    clearCookie(secure) {
      return `${COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0${secure ? '; Secure' : ''}`;
    },
    // `?t=` for a path: valid for `ttlSeconds`, for that path only.
    signPath(pathname, ttlSeconds = 3600) {
      const exp = Math.floor(now() / 1000) + ttlSeconds;
      const mac = crypto.createHmac('sha256', secret).update(`${exp}\n${pathname}`).digest('base64url');
      return `${exp}.${mac}`;
    },
    // A lasting token for `label` (the AltSource feed URL), stable until data/secret changes.
    derive(label) {
      return crypto.createHmac('sha256', secret).update(`derive\n${label}`).digest('base64url');
    },
    verifyPath(pathname, t) {
      const m = /^(\d{1,12})\.([A-Za-z0-9_-]{43})$/.exec(String(t || ''));
      if (!m || Number(m[1]) < now() / 1000) return false;
      const expected = crypto.createHmac('sha256', secret).update(`${m[1]}\n${pathname}`).digest();
      const given = Buffer.from(m[2], 'base64url');
      return given.length === expected.length && crypto.timingSafeEqual(given, expected);
    },
  };
}

module.exports = { createAuth, hashPassword, verifyPassword, parseCookies, COOKIE };
