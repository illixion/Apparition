// Shared by Apparition Store and the desktop Developer tab. The shell supplies
// authenticated actions; this component owns presentation, choices and feedback.
(function(root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory(require('../freshness.js'));
  else root.ApparitionProjects = factory(root.Freshness);
})(globalThis, function(Freshness) {
'use strict';
const {esc, agoFrom} = Freshness;
const ago = t => agoFrom(Date.now(), t);
const targetKey = p => JSON.stringify([p.device, p.platform]);
class ProjectList {
  constructor(root, options = {}) {
    this.root = root; this.options = options; this.selections = options.selections || {};
    this.pending = new Set(); this.messages = new Map(); this.query = ''; this.sort = 'name';
    root.classList.add('apparition-projects');
    root.innerHTML = `<div class="project-tools"><input type="search" placeholder="Search projects, devices or platforms" aria-label="Search projects"><select aria-label="Sort projects"><option value="name">Project name</option><option value="attention">Needs update first</option><option value="activity">Recent activity</option></select><span class="project-count" aria-live="polite"></span></div><div class="project-list"></div>`;
    this.list = root.querySelector('.project-list');
    root.querySelector('input').addEventListener('input', e => { this.query = e.target.value; this.render(); });
    root.querySelector('select').addEventListener('change', e => { this.sort = e.target.value; this.render(); });
    root.addEventListener('change', e => {
      const t = e.target;
      if (t.dataset.chip) {
        const s = this.selections[t.dataset.chip], key = targetKey(t.dataset);
        if (t.checked) s.chosen.add(key); else s.chosen.delete(key);
      } else if (t.dataset.repo && t.dataset.k) {
        this.selections[t.dataset.repo][t.dataset.k] = t.type === 'checkbox' ? t.checked : t.value;
      } else return;
      e.stopPropagation(); this.render();
    });
    root.addEventListener('click', e => {
      const control = e.target.closest('[data-submit]');
      if (!control) return;
      e.stopPropagation();
      const repo = this.state.repos.find(r => r.name === control.closest('[data-project]').dataset.project);
      this.submit(repo, control.dataset.submit === 'install');
    });
  }
  reset() {
    this.generation = (this.generation || 0) + 1;
    this.selections = {}; this.pending = new Set(); this.messages = new Map();
    this.signature = null; this.state = null;
  }
  selection(repo) {
    return this.selections[repo.name] ||= { branch:'current', chosen:new Set(), ...repo.deployDefaults };
  }
  update(state) {
    this.state = { ...state, installs:state.installs || {}, deviceApps:state.deviceApps || {}, repos:state.repos || [] };
    for (const [name, message] of this.messages) {
      const ids = message.match(/#(\d+)/g);
      if (ids && ids.every(id => (state.builds || []).some(j => j.id === Number(id.slice(1))))) this.messages.delete(name);
    }
    this.render();
  }
  async submit(repo, deploy) {
    if (this.pending.has(repo.name)) return;
    const generation = this.generation;
    const s = this.selection(repo);
    const targets = repo.platforms.filter(p => s.chosen.has(targetKey(p))).map(p => ({ device:p.device, platform:p.platform, branch:repo.deviceBranches?.[p.device] || s.branch }));
    if (!targets.length) return;
    this.pending.add(repo.name); this.messages.set(repo.name, deploy ? 'Starting build and install…' : 'Starting build…'); this.render();
    try {
      const reply = await this.options.build(repo, targets, {run:!!s.launch, mcp:!!s.mcp, jit:!!s.jit, buildDebug:!!s.buildDebug, inject:!!s.inject, injectBackend:s.injectBackend || 'dynamic', ...(deploy ? {} : {deploy:false, run:false})});
      if (generation !== this.generation) return;
      if (reply?.error) throw new Error(reply.error);
      this.messages.set(repo.name, reply?.jobs?.length ? `Queued ${reply.jobs.map(j => '#' + j.id).join(', ')}. Build request accepted.` : 'Build request accepted.');
      this.options.queued?.(reply);
    } catch(error) { if (generation === this.generation) this.messages.set(repo.name, `Could not start: ${error.message || error}`); }
    finally { if (generation === this.generation) { this.pending.delete(repo.name); this.render(); } }
  }
  render() {
    if (!this.state) return;
    const S = this.state, live = (S.debugApps?.apps || []).filter(a => a.attachable);
    const activity = r => Math.max(0, ...(S.builds || []).filter(b => b.repo === r.name).map(b => b.queuedAt || b.startedAt || b.id));
    const attention = r => Math.max(0, ...r.platforms.map(p => ({bad:3,warn:2,muted:1}[Freshness.platformStatus(p,r.git || {},S).dot] || 0)));
    const q = this.query.trim().toLocaleLowerCase();
    const repos = S.repos.filter(r => [r.name,r.git?.branch,...r.platforms.flatMap(p => [p.device,p.platform])].join(' ').toLocaleLowerCase().includes(q)).sort((a,b) =>
      (this.sort === 'activity' ? activity(b)-activity(a) : this.sort === 'attention' ? attention(b)-attention(a) : 0) || a.name.localeCompare(b.name,undefined,{sensitivity:'base'}));
    // Keep focused controls in place on unchanged polls (including while typing).
    const signature = JSON.stringify([repos,S.installs,S.deviceApps,S.ledgerOnly,S.ota,S.builds,S.running,S.queued,live,this.selections,[...this.pending],[...this.messages],this.query,this.sort,S.nativeExtras,this.options.entryPatching], (k,v) => v instanceof Set ? [...v] : v);
    if (signature === this.signature) return;
    this.signature = signature;
    this.root.querySelector('.project-count').textContent = `${repos.length} of ${S.repos.length} projects`;
    this.list.innerHTML = repos.map(r => this.markup(r,S,live)).join('') || '<p>No matching projects.</p>';
    for (const card of this.list.querySelectorAll('[data-project]')) {
      this.options.extend?.(card, S.repos.find(r => r.name === card.dataset.project));
    }
  }
  markup(r,S,live) {
    const g = r.git||{};
    const s = this.selection(r);
    const branches = ['current', ...(r.branches || []).filter(b=>b!=='current')];
    const runningBuild = (S.builds || []).find(b => (S.running||[]).includes(b.id) && b.repo===r.name && !b.macApp);
    const queuedIds = new Set((S.builds||[]).filter(b=>(S.queued||[]).includes(b.id) && b.repo===r.name && !b.macApp).map(targetKey));
    const latestBuild = (S.builds || []).find(b => b.repo === r.name);
    const busy = !!runningBuild || queuedIds.size > 0 || this.pending.has(r.name);

    const chips = r.platforms.map(p=>{
      const st = Freshness.platformStatus(p, g, S);
      const branchOverride = r.deviceBranches?.[p.device];
      const isSelected = s.chosen.has(targetKey(p));
      const isRunningHere = runningBuild && runningBuild.device === p.device && runningBuild.platform === p.platform;
      const isQueuedHere = queuedIds.has(targetKey(p));
      const https = this.options.ota;
      let otaIcon = '';
      if (https) {
        const title = st.ota
          ? `Rebuild ${p.platform} without touching the device, then offer the OTA install link`
          : `Build ${p.platform} for OTA install (no OTA copy published yet)`;
        otaIcon = `<span class="otaicon" data-otabuild="${esc(r.name)}" data-device="${esc(p.device)}" title="${esc(title)}">⤓</span>`;
      }
      const running = live.find(a => a.bundleId === p.bundleId && String(a.device).toLowerCase() === String(p.device).toLowerCase());
      const liveMark = running && this.options.liveLinks !== false ? `<a class="live" href="#debug/${encodeURIComponent(running.id)}" title="Running now with its debug server: open the live log">live</a>` : '';
      const title = `${esc(p.device)} · ${esc(p.platform)}\n${esc(st.detail)}`;
      return `<label class="chip ${isSelected?'sel':''} ${isRunningHere||isQueuedHere?'building':''}" title="${title}">
        <input type="checkbox" data-chip="${esc(r.name)}" data-device="${esc(p.device)}" data-platform="${esc(p.platform)}" ${isSelected?'checked':''}>
        <span class="dot ${st.dot}"></span><span class="target-status">${esc(st.label)}</span>
        <span class="devname">${esc(p.device)}</span>
        <span class="platname">${esc(p.platform)}</span>
        ${branchOverride ? `<span class="branchtag">${esc(branchOverride)}</span>` : ''}
        ${liveMark}
        ${otaIcon}
      </label>`;
    }).join('');

    // Mac apps download as a signed .app.zip of the selected branch's committed
    // head, built on demand. Only the checked-out branch's head is known here,
    // so only that branch can say whether the last build is current.
    const macBranch = s.branch === 'current' ? g.branch : s.branch;
    const macs = (this.options.macDownloads === false ? [] : r.macApps || []).map(m => {
      const last = (m.built || []).find(e => e.branch === macBranch) || (m.built || [])[0];
      const knownHead = macBranch === g.branch && g.commit;
      const fresh = last && knownHead && last.branch === macBranch && (last.commit.startsWith(g.commit) || g.commit.startsWith(last.commit));
      const dot = fresh ? 'ok' : last && knownHead ? 'warn' : '';
      const building = (m.building || []).length > 0;
      const info = last
        ? `${last.branch}@${last.commit} · ${(last.size/1048576).toFixed(1)} MB · ${ago(last.at)}${fresh ? '' : knownHead ? ` · head ${g.commit} builds on download` : ''}`
        : 'not built yet: the first download builds it';
      const title = `${m.bundleId} · PLATFORM=${m.platform}\nBuilds the committed head of the selected branch (never uncommitted changes), signed with the development certificate.\n`
        + `Opened from a browser download, macOS blocks it once: System Settings › Privacy & Security › Open Anyway (or xattr -dr com.apple.quarantine on the .app).`;
      return `<div class="macapp ${building?'building':''}" title="${esc(title)}">
        <span class="dot ${dot}"></span>
        <span class="macname">${esc(m.name)}</span>
        <span class="platname">${esc(m.platform)}</span>
        <span class="macbuilt">${esc(info)}</span>
        <button class="ghost" data-maclink="${esc(m.slug)}" data-repo="${esc(r.name)}" title="Copy a curl command that downloads the head of ${esc(macBranch)} on another Mac (signed link, no sign-in, no quarantine prompt; good for a week)">curl</button>
        <button data-macdl="${esc(m.slug)}" data-repo="${esc(r.name)}" ${building?'disabled':''}>${building ? 'Building…' : '⤓ .app.zip'}</button>
      </div>`;
    }).join('');

    const chosenCount = r.platforms.filter(p => s.chosen.has(targetKey(p))).length;
    const hasDevices = r.platforms.length > 0;
    return `<article class="card" data-project="${esc(r.name)}" aria-busy="${busy}">
      <div class="cardhead">
        <span class="reponame">${esc(r.name)}</span>
        <span class="refline">${esc(g.branch)}@<code>${esc(g.commit)}</code>${g.dirty?' <span class="dirty">●dirty</span>':''}</span>
        <span class="subject">${esc(g.subject||'')}</span>
      </div>
      ${hasDevices ? `<div class="chips">${chips}</div>` : ''}
      ${macs ? `<div class="macapps">${macs}</div>` : ''}
      <div class="cardfoot">
        <select class="branchsel" aria-label="${esc(r.name)} branch" data-repo="${esc(r.name)}" data-k="branch">
          ${branches.map(b=>`<option value="${esc(b)}" ${s.branch===b?'selected':''}>${esc(b === 'current' ? `Current checkout · ${g.branch || 'unknown'}` : b)}</option>`).join('')}
        </select>
        ${hasDevices ? `<div class="opts" role="group" aria-label="Deploy options">
          <label class="runcheck" title="--run: launch the app on the device once it is installed. Unticked, it is installed and updates in the background">
            <input type="checkbox" data-repo="${esc(r.name)}" data-k="launch" ${s.launch?'checked':''}>Launch app</label>
          <label class="runcheck" title="--mcp: make the launch a debug session (console attached, session registered for agents)${s.launch?'':'. Applies only with Launch app'}">
            <input type="checkbox" data-repo="${esc(r.name)}" data-k="mcp" ${s.mcp?'checked':''} ${s.launch?'':'disabled'}>MCP</label>
          <label class="runcheck" title="--jit: launch through Apparition, which serves the app's JIT requests (emulators, Wine, JIT VMs) for as long as it runs. The device must be paired with Apparition on this Mac${s.launch?'':'. Applies only with Launch app'}">
            <input type="checkbox" data-repo="${esc(r.name)}" data-k="jit" ${s.jit?'checked':''} ${s.launch?'':'disabled'}>JIT</label>
          <label class="runcheck" title="--debug: build the Debug configuration instead of Release">
            <input type="checkbox" data-repo="${esc(r.name)}" data-k="buildDebug" ${s.buildDebug?'checked':''}>Debug build</label>
          <label class="runcheck" title="--inject: build so running code can be patched live (development builds only)">
            <input type="checkbox" data-repo="${esc(r.name)}" data-k="inject" ${s.inject?'checked':''}>Inject</label>
        </div>
        ${this.options.entryPatching ? `<label>Patch backend <select data-repo="${esc(r.name)}" data-k="injectBackend"><option value="dynamic" ${s.injectBackend !== 'apparition' ? 'selected' : ''}>Swift dynamic replacement</option><option value="apparition" ${s.injectBackend === 'apparition' ? 'selected' : ''}>Apparition entry boundaries</option></select></label>` : ''}
        <span class="spacer"></span>
        <button data-submit="build" ${busy||chosenCount===0?'disabled':''}>Build only</button>
        <button class="primary" data-submit="install" ${busy||chosenCount===0?'disabled':''}>
          ${busy ? 'Working…' : chosenCount>1 ? `Install on ${chosenCount} targets` : 'Install selected'}
        </button>` : ''}
      </div>
      <p class="project-progress" role="status" ${busy || latestBuild || this.messages.has(r.name) ? '' : 'hidden'}>${esc(this.messages.get(r.name) || (runningBuild ? `Building #${runningBuild.id} · ${runningBuild.device || runningBuild.platform}` : queuedIds.size ? 'Queued for build' : latestBuild ? `#${latestBuild.id} · ${latestBuild.status} · ${latestBuild.device || latestBuild.platform}` : ''))}</p>
    </article>`;

  }
}
return {ProjectList, targetKey};
});
