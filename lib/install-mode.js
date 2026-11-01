'use strict';
// How a build reaches its device, and whether the device's own Apparition is open to take it.
//
//   mac     build-and-sign installs through `xcrun devicectl` (the Xcode tunnel). Quick, keeps
//           --run/--mcp launches working, but the Mac must reach the device on the local network
//           (or a tunnel it has set up).
//   device  the build is only built and kept; the Apparition app on the device downloads the IPA
//           over the service's HTTPS address and signs and installs it itself. Needs nothing from
//           the Mac's network, so it works over Tailscale and with a free Apple ID.
//   none    build and keep only.
//   auto    `device` while that device's Apparition is open (it identifies itself on every request
//           it makes), otherwise `mac`. A closed Apparition cannot take an install, so the tunnel
//           is the fallback and stays the way a terminal or the web page deploys.
//
// A request that names none of these keeps the old meaning: `deploy: false` is `none`, anything
// else is `mac`.

const MODES = ['auto', 'mac', 'device', 'none'];
// Apparition polls the service every few seconds while it is in front; a quiet client is closed.
const PRESENCE_TTL_MS = 20000;
const NAME = /^[^\x00-\x1f\x7f]{1,128}$/;

const key = name => String(name).trim().toLowerCase();

function createPresence({ ttl = PRESENCE_TTL_MS, max = 64 } = {}) {
  const seen = new Map();
  return {
    // The device name a client sends (Xcode's name for it); anything else is ignored.
    touch(name, now = Date.now()) {
      if (typeof name !== 'string' || !NAME.test(name.trim()) || !name.trim()) return false;
      seen.delete(key(name));
      seen.set(key(name), now);
      for (const [k, at] of seen) if (now - at > ttl * 3 || seen.size > max) seen.delete(k);
      return true;
    },
    listening(name, now = Date.now()) {
      const at = typeof name === 'string' ? seen.get(key(name)) : undefined;
      return at !== undefined && now - at <= ttl;
    },
    // Names of devices whose Apparition is open now, for the state a client reads.
    open(now = Date.now()) { return [...seen].filter(([, at]) => now - at <= ttl).map(([k]) => k); },
  };
}

function validInstallMode(value) { return value === undefined || MODES.includes(value); }

// The mode a build job gets. `device` is the target's device name.
function resolveInstallMode({ install, deploy, device, presence, now = Date.now() }) {
  if (!validInstallMode(install)) throw new Error(`install must be one of ${MODES.join(', ')}`);
  const requested = install !== undefined ? install : deploy === false ? 'none' : 'mac';
  if (requested !== 'auto') return requested;
  return presence?.listening(device, now) ? 'device' : 'mac';
}

module.exports = { MODES, PRESENCE_TTL_MS, createPresence, validInstallMode, resolveInstallMode };
