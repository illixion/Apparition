// Freshness of one device/platform row. Loaded by index.html in the browser and
// by test/freshness.test.js in Node.
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.Freshness = factory();
})(typeof self !== 'undefined' ? self : this, function () {
'use strict';

function esc(s){ return String(s??'').replace(/[&<>"]/g, c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }

function agoFrom(now, t){ if(!t) return '—'; const s=(now-t)/1000;
  if(s<60) return Math.round(s)+'s ago'; if(s<3600) return Math.round(s/60)+'m ago';
  if(s<86400) return Math.round(s/3600)+'h ago'; return Math.round(s/86400)+'d ago'; }

// Dev builds keep a static version string ("1.0"), so freshness has to be
// judged from the git short SHA stamped into CFBundleVersion (optionally
// "-dirty"), not the version. Returns a compact {dot,label,detail} plus
// whatever's needed to draw the OTA reinstall affordance for this platform.
function platformStatus(p, g, S, now = Date.now()) {
  const inv = S.deviceApps[p.device];
  const dev = inv?.apps?.[p.bundleId];
  const led = S.installs[p.device]?.[p.bundleId];
  const ota = S.ota?.[p.bundleId + '|' + p.platform];
  const isSha = s => /^[0-9a-f]{7,40}$/i.test(s || '');
  const rawBuild = dev?.buildNo || '';
  const devDirty = /-dirty$/i.test(rawBuild);
  const devSha = rawBuild.replace(/-dirty$/i, '');
  const onSha = isSha(devSha) ? devSha : (led && isSha(led.commit) ? led.commit : '');
  const head = g.commit || '';
  const comparable = isSha(onSha) && isSha(head);
  const match = comparable && (onSha === head || head.startsWith(onSha) || onSha.startsWith(head));
  const otherBranch = led?.branch && g.branch && led.branch !== g.branch;

  let dot = 'muted', label = 'no build', detail = 'Store has no recorded build or install for this target yet.';
  if (inv?.error) {
    dot = 'bad'; label = 'inventory error';
    detail = `devicectl inventory failed ${agoFrom(now, inv.erroredAt)}: ${inv.error}`;
  } else if (!dev) {
    if (inv) { dot = 'muted'; label = 'not installed'; detail = 'Not present on this device per the last devicectl inventory.'; }
    else { dot = 'muted'; label = 'pending…'; detail = 'Device inventory has not been fetched yet.'; }
  } else {
    const ver = `v${dev.version}`;
    if (!comparable) {
      dot = 'info'; label = ver;
      detail = `On device: ${ver}${dev.buildNo ? ` (${dev.buildNo})` : ''}. Build number isn't a recognizable git SHA, so freshness can't be checked.`;
    } else if (match && !devDirty && !g.dirty) {
      dot = 'ok'; label = 'up to date';
      detail = `On device: ${ver} @${devSha}, reported ${agoFrom(now, inv.fetchedAt)}. Matches ${esc(g.branch)}@${head}.`;
    } else if (match) {
      dot = 'warn'; label = 'dirty match';
      detail = `On device: ${ver} @${devSha}${devDirty ? ' (dirty build)' : ''}. Commit matches, but the source has uncommitted changes.`;
    } else if (otherBranch) {
      dot = 'info'; label = `from ${led.branch}`;
      detail = `On device: ${ver} @${devSha}. Built from ${led.branch}, not the currently selected ${g.branch}.`;
    } else {
      dot = 'bad'; label = 'out of date';
      detail = `On device: ${ver} @${devSha}${devDirty ? ' (dirty)' : ''}. Repo HEAD is now ${head} — deploy to update.`;
    }
  }
  if (led) {
    detail += led.confirmed === false
      ? ` Store's last action: OTA build ${led.branch}@${led.commit} offered ${agoFrom(now, led.at)} — not confirmed installed.`
      : ` Store's last action: LAN install of ${led.branch}@${led.commit} confirmed ${agoFrom(now, led.at)}.`;
  }
  return { dot, label, detail, ota };
}

return { platformStatus, agoFrom, esc };
});
