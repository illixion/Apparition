// The build service is reached only through Rust. No cookies, bearer tokens or
// remote HTML enter this view; all service-provided strings become text nodes.
//
// Layout: before connecting, a getting-started page (what Apparition Store is, where to get it, how
// to connect). Once connected, four sections: Projects (a two-column grid of cards, one tile per
// target with what is installed there and what can be done), Activity (the build queue and
// logs), Reports (crashes, traces and running debug apps, grouped by app) and Devices &
// signing (with a note on Apparition Store itself).
//
// Rendering rule: the view polls, so a section repaints only when what it shows has changed, and
// what the person has opened (a report group, a kept-builds list, a log) is remembered across
// repaints. A log, report or build manifest opens inline, directly under the row that asked.
const element = (tag, className = '', text = '') => {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
};
const button = (text, action, className = 'button quiet') => {
  const node = element('button', className, text);
  node.type = 'button';
  node.addEventListener('click', action);
  return node;
};
const pill = (text, tone = '', title = '') => {
  const node = element('span', `pill ${tone}`.trim(), text);
  if (title) node.title = title;
  return node;
};
const statusTone = status => ({ running: 'info', queued: 'info', success: 'good', failed: 'bad' })[status] || '';
const plural = (n, noun) => `${n} ${noun}${n === 1 ? '' : noun.endsWith('h') ? 'es' : 's'}`;
const ago = ms => {
  if (!ms) return '';
  const minutes = Math.round((Date.now() - ms) / 60000);
  if (minutes < 1) return 'just now';
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  return hours < 48 ? `${hours} h ago` : `${Math.round(hours / 24)} days ago`;
};
const ACTIVE = ['queued', 'running'];
export const SECTIONS = [['projects', 'Projects'], ['activity', 'Activity'], ['reports', 'Reports'], ['setup', 'Devices & signing']];

// What a job is, in words, from the flags the service sets on it.
export function jobKind(job) {
  if (job.resign) return job.resign.dryRun ? 'Re-sign check' : 'Re-sign';
  if (job.forResign) return 'Build for re-sign';
  if (job.altSource) return 'Source download build';
  if (job.macApp) return 'Mac download build';
  return job.noDeploy ? 'Build only' : 'Build & install';
}

// Where a project target stands on its device, from the service's install ledger and git state.
export function targetState(repo, target, installs = {}) {
  const install = installs?.[target.device]?.[target.bundleId];
  if (!install) return { install: null, text: 'Not installed from this Mac yet' };
  const head = repo.git?.commit;
  const parts = [`Installed ${install.commit || install.buildNo || install.buildId || 'a build'}`];
  if (install.resigned) parts.push(`re-signed with ${install.resigned}`);
  else if (install.confirmed === false) parts.push('offered as a download, not confirmed');
  if (head && install.commit && install.branch === repo.git?.branch) parts.push(install.commit === head ? 'up to date' : `checkout is at ${head}`);
  if (install.at) parts.push(ago(install.at));
  return { install, text: parts.join(' · ') };
}

// The same, as a badge for the target's tile: scannable where the line above is detailed.
export function targetBadge(repo, target, installs = {}) {
  const install = installs?.[target.device]?.[target.bundleId];
  if (!install) return { tone: '', text: 'Not installed' };
  const head = repo.git?.commit;
  if (head && install.commit && install.branch === repo.git?.branch) {
    return install.commit === head ? { tone: 'good', text: 'Up to date' } : { tone: 'warn', text: 'Behind checkout' };
  }
  return { tone: 'info', text: 'Installed' };
}

export class DeveloperView {
  constructor(root, invoke) {
    this.root = root;
    this.invoke = invoke;
    this.connected = false;
    this.visible = false;
    this.polling = false;
    // The job whose log is being followed (set when a build is queued from here).
    this.selectedLog = null;
    this.projectSignature = '';
    // Per-section signatures of what was last painted, the rows that can host the inline viewer,
    // and what stays open across repaints.
    this.signatures = {};
    this.hosts = new Map();
    this.viewing = null;
    this.last = null;
    this.openGroups = new Set();
    this.expandedGroups = new Set();
    this.epoch = 0;
    this.capabilities = new Set();
    this.repos = [];
    this.artifactList = [];
    this.installs = {};
    this.jobsList = [];
    this.section = 'projects';
    this.showAllJobs = false;
    root.append(element('h1', '', 'Developer'));
    root.append(element('p', 'lede', 'Build and iterate on your Apple-device apps, install them through Apparition, and keep development builds refreshed beyond the free account window. Launch with JIT, apply live Swift patches, and inspect reports from the project.'));
    this.intro = this.introCard();
    this.hostCard = this.hostSection();
    const form = element('form', 'dev-card dev-connect');
    this.remoteForm = form;
    const fields = element('div', 'dev-connect-fields');
    const addressLabel = element('label', '', 'Build service');
    this.address = element('input');
    this.address.type = 'url'; this.address.required = true;
    this.address.placeholder = 'https://your-mac.example.com';
    this.address.value = 'http://localhost:8484';
    addressLabel.append(this.address);
    const passwordLabel = element('label', '', 'Service password');
    this.password = element('input');
    this.password.type = 'password'; this.password.required = true;
    this.password.autocomplete = 'current-password';
    passwordLabel.append(this.password);
    this.connect = element('button', 'button primary', 'Connect');
    this.connect.type = 'submit';
    fields.append(addressLabel, passwordLabel, this.connect);
    this.hint = element('p', 'hint', 'The password is the one set with node server.js set-password in the Apparition Store. On this Mac, http://localhost:8484 works; elsewhere use its HTTPS address. Apparition forgets the session when it quits.');
    this.fields = fields;
    // Once connected the form folds into one line: where, and a way out.
    this.connected_bar = element('div', 'dev-connected'); this.connected_bar.hidden = true;
    this.connected_label = element('span', 'dev-connected-label selectable');
    this.disconnect = button('Disconnect', () => this.signOut());
    this.connected_bar.append(element('span', 'dot on'), this.connected_label,
      button('Apparition Store on GitHub', () => this.open('store'), 'button quiet small'), this.disconnect);
    form.append(fields, this.hint, this.connected_bar);
    form.addEventListener('submit', event => { event.preventDefault(); this.signIn(); });
    this.signing = this.signingSection();
    this.note = element('p', 'status'); this.note.setAttribute('role', 'status');

    this.content = element('div', 'dev-content'); this.content.hidden = true;
    this.nav = element('div', 'dev-sections'); this.nav.setAttribute('role', 'tablist');
    this.nav.setAttribute('aria-label', 'Developer sections');
    this.navButtons = {};
    for (const [key, title] of SECTIONS) {
      const tab = button(title, () => this.select(key), 'dev-section-tab');
      tab.setAttribute('role', 'tab');
      this.navButtons[key] = tab; this.nav.append(tab);
    }
    this.panes = {};
    for (const [key] of SECTIONS) { this.panes[key] = element('div', 'dev-pane'); this.panes[key].setAttribute('role', 'tabpanel'); }

    this.resign = this.resignSection();
    this.projects = this.block('Projects', 'Each tile is a device the project installs on. Install builds and installs it there; Build only keeps a signed IPA to download or re-sign. Device-side installs retain an unsigned IPA for Apparition to sign on the device.', true);
    this.registration = this.registrationForm(); this.registration.hidden = true;
    this.artifacts = this.block('Other kept builds', 'Builds the Mac kept that no project here installs, such as Xcode products registered by hand.');
    this.artifacts.hidden = true;
    this.panes.projects.append(this.resign, this.projects, this.registration, this.artifacts);

    this.jobs = this.block('Builds', 'Newest first. Builds for one project run one at a time.');
    this.panes.activity.append(this.jobs);

    this.reports = this.block('Crash logs and traces', 'Pulled from your devices and uploaded by apps that link DebugTrace, grouped by app.');
    this.debug = this.block('Running apps', 'Dev builds with a debug server that are running now. Inspecting one asks for approval on the device. Console opens its live log, tuning panels (sliders for values an app reads through DebugTunables, and panels an agent built), profiler and memory view.');
    this.panes.reports.append(this.reports, this.debug);

    this.devices = this.block('Devices', 'Devices this Mac can install to. Simulators are not listed.', true);
    this.about = this.aboutSection();
    this.panes.setup.append(this.devices, this.about);

    this.buildViewer();
    this.content.append(this.nav, ...SECTIONS.map(([key]) => this.panes[key]));
    this.webUi = this.webUiSection();
    root.append(this.hostCard, this.intro, form, this.note, this.content, this.signing);
    this.select('projects');
    invoke('service_settings').then(settings => { if (settings.url) this.address.value = settings.url; }).catch(() => {});
    this.hostCheck(true);
  }

  // What this is and how to start, for someone who has never run the Apparition Store.
  introCard() {
    const card = element('section', 'dev-card dev-intro');
    card.append(element('h2', 'small', 'Get started'));
    card.append(element('p', '', 'Apparition Store is an open-source build service that runs on a Mac with Xcode. It builds your projects and keeps the artifacts. Install through Apparition on the device to sign with that device’s Apple ID or certificate; direct Mac installs use the Mac’s development certificate and profile. The service also collects debug traces and crash reports. Apparition connects to it from this Mac or from a headset, iPhone or Apple TV.'));
    const steps = element('ol', 'dev-steps');
    for (const text of [
      'Get Apparition Store from GitHub and follow its README. The build host needs macOS, Xcode and Node 18. A paid certificate and development profile are only needed for direct Mac installs; device-side installs use the identity in Apparition on the device.',
      'Set a service password: node server.js set-password.',
      'Start it (node server.js, or as a launch agent), then connect below. Register your projects here once connected.',
    ]) steps.append(element('li', '', text));
    const links = element('div', 'service-row');
    links.append(button('Apparition Store on GitHub', () => this.open('store')), button('Developer guide', () => this.open('guide')));
    card.append(steps, links);
    return card;
  }
  // Apparition Store is a second backend, a separate project, so say so once connected as well as before.
  aboutSection() {
    const card = element('section', 'dev-card dev-about');
    card.append(element('h2', 'small', 'About Apparition Store'));
    card.append(element('p', '', 'Apparition Store is an open-source build service that runs on a Mac with Xcode. It builds your projects and keeps artifacts. Device-side installs are signed by Apparition on the device; direct Mac installs use the Mac’s development certificate and profile. The service also re-signs apps with another profile and collects debug traces and crash reports.'));
    const links = element('div', 'service-row');
    links.append(button('Apparition Store on GitHub', () => this.open('store'), 'button quiet small'), button('Developer guide', () => this.open('guide'), 'button quiet small'));
    card.append(links);
    return card;
  }

  // The inline viewer: one box that moves under whichever row asked for it, so a log, report or
  // manifest appears where the button was instead of at the foot of the page.
  buildViewer() {
    this.viewer = element('div', 'dev-viewer'); this.viewer.hidden = true;
    const head = element('div', 'dev-viewer-head');
    this.viewerTitle = element('strong');
    this.viewerCopy = button('Copy', () => this.copyViewer(), 'button quiet small');
    head.append(this.viewerTitle, this.viewerCopy, button('Close', () => this.closeView(), 'button quiet small'));
    this.detail = element('pre', 'service-detail selectable');
    this.detail.tabIndex = 0;
    this.detail.addEventListener('scroll', () => { this.viewerScroll = this.detail.scrollTop; });
    this.viewer.append(head, this.detail);
  }
  host(rowId, node) { this.hosts.set(rowId, node); return node; }
  viewKey() { return this.viewing ? `${this.viewing.rowId}|${this.viewing.id}` : ''; }
  isViewing(rowId, id) { return this.viewing?.rowId === rowId && this.viewing.id === id; }
  // Move the viewer under its row if the row exists; hide it if not. Re-attaching a node resets its
  // scroll position, so only move it when its row is a different node.
  placeViewer() {
    const host = this.viewing && this.hosts.get(this.viewing.rowId);
    if (!host) { this.viewer.remove?.(); this.viewer.hidden = true; return; }
    this.viewer.hidden = false;
    if (this.viewer.parentNode !== host) {
      host.append(this.viewer);
      if (this.viewerScroll) this.detail.scrollTop = this.viewerScroll;
    }
  }
  setViewer(title, text) {
    const pre = this.detail, view = this.viewing;
    this.viewerTitle.textContent = title;
    const atEnd = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 32;
    const first = view && !view.filled;
    if (pre.textContent !== text) pre.textContent = text;
    if (view && text) view.filled = true;
    if (first || atEnd) pre.scrollTop = view?.live ? pre.scrollHeight : first ? 0 : pre.scrollTop;
    this.viewerScroll = pre.scrollTop;
  }
  copyViewer() {
    navigator.clipboard?.writeText(this.detail.textContent).then(() => {
      this.viewerCopy.textContent = 'Copied';
      setTimeout(() => { this.viewerCopy.textContent = 'Copy'; }, 1500);
    }).catch(() => {});
  }
  // A button that opens (and, pressed again, closes) something inline under its row.
  viewButton(rowId, id, show, hide, spec) {
    const control = button(this.isViewing(rowId, id) ? hide : show, () => this.toggleView(rowId, id, spec), 'button quiet small');
    control.setAttribute('aria-expanded', String(this.isViewing(rowId, id)));
    return control;
  }
  async toggleView(rowId, id, spec) {
    if (this.isViewing(rowId, id)) { this.closeView(); return; }
    this.viewing = { rowId, id, ...spec };
    this.selectedLog = spec.operation === 'buildLog' ? spec.args.id : null;
    this.viewerScroll = 0;
    this.setViewer(spec.title, 'Loading…');
    this.redraw();
    this.viewer.scrollIntoView?.({ block: 'nearest', behavior: 'smooth' });
    await this.loadViewing();
  }
  // Follow a job that was just queued: its log is open in Activity as soon as the person looks.
  followJob(id) {
    this.selectedLog = id;
    this.viewing = { rowId: `job:${id}`, id: 'log', title: `Build #${id}`, operation: 'buildLog', args: { id }, live: true };
    this.viewerScroll = 0;
    this.setViewer(this.viewing.title, '');
    this.redraw();
  }
  closeView() {
    this.viewing = null; this.selectedLog = null;
    this.redraw();
  }
  async loadViewing() {
    const view = this.viewing, epoch = this.epoch;
    if (!view) return;
    try {
      const reply = await this.request(view.operation, view.args);
      if (this.viewing !== view || epoch !== this.epoch || !this.connected) return;
      const text = typeof reply === 'string' ? reply : reply?.chunk ?? reply?.text ?? JSON.stringify(reply, null, 2);
      if (view.live) view.done = !ACTIVE.includes(reply?.status);
      this.setViewer(reply?.status && view.live ? `${view.title} · ${reply.status}` : view.title, text);
    } catch (error) {
      if (this.viewing === view) this.setViewer(view.title, `Could not load this: ${error?.message || error}`);
    }
  }
  // Repaint every section from the last data, e.g. after the inline viewer moved.
  redraw() {
    this.signatures = {}; this.projectSignature = '';
    this.renderAll();
    this.placeViewer();
  }
  // Skip the repaint when nothing a section shows has changed; open state survives either way.
  paint(key, signature, build) {
    if (this.signatures[key] === signature) return false;
    this.signatures[key] = signature;
    build();
    this.placeViewer();
    return true;
  }
  renderAll() {
    const last = this.last;
    if (!last) return;
    this.renderProjects(last.repos);
    this.renderDevices(this.deviceList || []);
    this.renderJobs(this.jobsList);
    this.renderReports(last.traces, last.crashes);
    this.renderDebug(last.debug);
    this.renderArtifacts(last.artifacts, this.jobsList);
  }

  open(which) { this.invoke('open_developer_link', { which }).catch(error => this.status(String(error?.message || error), true)); }

  select(key) {
    this.section = key;
    for (const [name] of SECTIONS) {
      this.panes[name].hidden = name !== key;
      this.navButtons[name].setAttribute('aria-selected', String(name === key));
    }
  }

  // Send build-and-sign's certificates and profiles to Apparition on a paired device, through
  // its Documents/Signing inbox. Needs no build service: it's this Mac's files and the device's
  // existing pairing.
  signingSection() {
    const card = element('section', 'dev-card dev-signing');
    card.hidden = true;
    card.append(element('h2', 'small', 'Signing on devices'));
    this.signingSummary = element('p', 'hint');
    const row = element('div', 'dev-signing-row');
    const label = element('label', '', 'Send to');
    this.signingDevice = element('select');
    this.signingDevice.setAttribute('aria-label', 'Device to send signing to');
    label.append(this.signingDevice);
    this.signingSend = element('button', 'button primary', 'Send certificates and profiles');
    this.signingSend.type = 'button';
    this.signingSend.addEventListener('click', () => this.sendSigning());
    row.append(label, this.signingSend);
    this.signingNote = element('p', 'status'); this.signingNote.hidden = true;
    card.append(this.signingSummary, row, this.signingNote);
    return card;
  }
  async loadSigning() {
    try {
      const [summary, devices] = await Promise.all([this.invoke('build_signing_summary'), this.invoke('paired_devices')]);
      this.signing.hidden = !summary;
      if (!summary) return;
      this.signingSummary.textContent = `build-and-sign on this Mac has ${plural(summary.certificates.length, 'certificate')} and ${plural(summary.profiles.length, 'profile')} (${summary.dir}). They go into Apparition's Signing folder on the device, so Apparition there can sign with your developer certificate; open it and enter each certificate's password once. Nothing is reinstalled.`;
      const previous = this.signingDevice.value;
      this.signingDevice.replaceChildren(...devices.map(device => {
        const option = element('option', '', `${device.name} · ${device.platform}`); option.value = device.udid; return option;
      }));
      if (devices.some(device => device.udid === previous)) this.signingDevice.value = previous;
      this.signingSend.disabled = !devices.length;
      if (!devices.length) this.signingSummary.textContent += ' Pair a device on the Install tab first.';
    } catch { this.signing.hidden = true; }
  }
  async sendSigning() {
    const note = (text, error = false) => { this.signingNote.hidden = !text; this.signingNote.textContent = text; this.signingNote.className = `status ${error ? 'error' : 'good'}`; };
    this.signingSend.disabled = true; note('Connecting to the device…');
    try {
      const sent = await this.invoke('hand_off_build_signing', { udid: this.signingDevice.value });
      note(`Sent ${sent.certificates} certificate(s) and ${sent.profiles} profile(s) to ${sent.device}. Open Apparition there and enter each .p12's password.`);
    } catch (error) { note(String(error?.message || error), true); }
    finally { this.signingSend.disabled = false; }
  }

  // Re-sign an app that is already on a device with another of the Mac's profiles. The source
  // is the exact IPA the Mac kept for it, or, when there is none, a fresh build of its project
  // target. The Mac does the work; nothing but identifiers and two choices leaves this view.
  resignSection() {
    const card = element('section', 'dev-card dev-resign');
    card.hidden = true;
    card.append(element('h2', 'small', 'Re-sign with another profile'));
    this.resignWhat = element('p', '');
    this.resignSource = element('p', 'hint');
    const grid = element('div', 'dev-resign-grid');
    const pick = (title, label) => {
      const wrap = element('label', '', title), select = element('select');
      select.setAttribute('aria-label', label); wrap.append(select); grid.append(wrap); return select;
    };
    this.resignDevice = pick('Device', 'Device to re-sign on');
    this.resignProfile = pick('Profile', 'Provisioning profile to sign with');
    this.resignData = pick('Data', 'What to do with the app\'s data');
    for (const [value, text] of [['keep', 'Keep its data'], ['discard', 'Discard its data']]) {
      const option = element('option', '', text); option.value = value; this.resignData.append(option);
    }
    this.resignData.value = 'keep';
    this.resignNote = element('p', 'hint',
      'A profile for the same App ID updates the app in place and keeps its data. A different App ID can\'t be installed over the old copy: the Mac then copies Documents and Library off the device, reinstalls, and puts them back (the backup stays on the Mac). Keychain items and app-group data do not move. The device must be reachable from the Mac.');
    const actions = element('div', 'service-row');
    this.resignCheck = button('Check first', () => this.submitResign(true));
    this.resignGo = element('button', 'button primary', 'Re-sign'); this.resignGo.type = 'button';
    this.resignGo.addEventListener('click', () => this.submitResign(false));
    actions.append(this.resignGo, this.resignCheck, button('Close', () => { card.hidden = true; this.resignTarget = null; }));
    card.append(this.resignWhat, this.resignSource, grid, this.resignNote, actions);
    return card;
  }
  resignDevices(build) {
    if (build.repo && build.device) return [build.device];
    const wanted = String(build.platform || '').toLowerCase();
    const names = new Set();
    for (const repo of this.repos || []) for (const target of repo.platforms || []) {
      if (target.bundleId === build.bundleId && String(target.platform).toLowerCase() === wanted) names.add(target.device);
    }
    return [...names];
  }
  async openResign(build) {
    if (!this.capabilities.has('resign')) return;
    const devices = this.resignDevices(build);
    this.resignTarget = build;
    this.resign.hidden = false;
    this.select('projects');
    this.resignWhat.textContent = `${build.name || build.bundleId} · ${build.platform || ''}${build.label ? ` · ${build.label}` : ''}`;
    this.resignSource.textContent = build.buildId
      ? `Signs the build the Mac kept (${build.buildId}).`
      : `The Mac kept no build of this app, so it builds ${build.repo} from its current checkout first (without installing it), then re-signs that build.`;
    this.resignDevice.replaceChildren(...devices.map(name => { const option = element('option', '', name); option.value = name; return option; }));
    this.resignGo.disabled = this.resignCheck.disabled = true;
    try {
      const { profiles = [] } = await this.request('resignOptions');
      const usable = profiles.filter(profile => !profile.expired);
      this.resignProfile.replaceChildren(...usable.map(profile => {
        const option = element('option', '', `${profile.name} · ${profile.appId}${profile.debuggable ? '' : ' · not debuggable'}`);
        option.value = profile.id; return option;
      }));
      if (!devices.length) this.status(`No configured project installs ${build.bundleId} on a ${build.platform} device, so there's nothing to sign it for.`, true);
      else if (!usable.length) this.status('No usable provisioning profiles in the Mac\'s build-signing folder.', true);
      else this.status('');
      this.resignGo.disabled = this.resignCheck.disabled = !devices.length || !usable.length;
    } catch (error) { this.status(String(error?.message || error), true); }
    this.resign.scrollIntoView?.({ block: 'nearest' });
  }
  async submitResign(dryRun) {
    const target = this.resignTarget;
    if (!target) return;
    this.resignGo.disabled = this.resignCheck.disabled = true;
    try {
      const source = target.buildId ? { id: target.buildId } : { repo: target.repo, platform: target.platform };
      const reply = await this.request('resign', { ...source, device: this.resignDevice.value, profile: this.resignProfile.value,
        dataMode: this.resignData.value, dryRun });
      // A re-sign without a kept build is two jobs, the build first; follow the re-sign.
      const job = reply?.jobs?.at(-1);
      if (job) {
        this.followJob(job.id);
        this.status(reply.jobs.length > 1 ? `Queued build #${reply.jobs[0].id}, then re-sign #${job.id}. Follow them in Activity.`
          : `Queued ${dryRun ? 'check' : 're-sign'} #${job.id}. Follow it in Activity.`);
      }
      await this.refresh();
    } catch (error) { this.status(String(error?.message || error), true); }
    finally { this.resignGo.disabled = this.resignCheck.disabled = false; }
  }
  resignButton(build) {
    return this.capabilities.has('resign') ? button('Re-sign…', () => this.openResign(build)) : null;
  }

  registrationForm() {
    const container = element('details', 'dev-card');
    container.append(element('summary', '', 'Register a project'));
    const form = element('form', 'service-project');
    const input = (title, placeholder = '') => {
      const label = element('label', '', title), field = element('input');
      field.required = true; field.placeholder = placeholder;
      label.append(field); form.append(label); return field;
    };
    const name = input('Project name', 'MyApp');
    const path = input('Checkout path on the Mac', '~/Projects/MyApp');
    const branches = input('Local branches, separated by commas', 'main'); branches.required = false;
    form.append(element('p', 'hint', 'Choose an existing Git checkout with build-signing.conf already set up (see Apparition Store\'s README). Registration saves its targets; it does not build or install anything.'));
    const targets = element('div', 'service-project'); form.append(targets);
    const rows = [];
    const addTarget = () => {
      const row = element('div', 'service-row'), fields = {};
      for (const [key, title, placeholder] of [['platform', 'Platform alias', 'visionOS'], ['device', 'Device name', 'Vision Pro'], ['bundleId', 'Bundle identifier', 'com.example.myapp']]) {
        const label = element('label', '', title), field = element('input');
        field.required = true; field.placeholder = placeholder; fields[key] = field;
        label.append(field); row.append(label);
      }
      const entry = { row, fields }; rows.push(entry);
      row.append(button('Remove target', () => {
        if (rows.length === 1) return;
        rows.splice(rows.indexOf(entry), 1); targets.replaceChildren(...rows.map(item => item.row));
      }));
      targets.append(row);
    };
    addTarget();
    const submit = element('button', 'button primary', 'Register project'); submit.type = 'submit';
    form.append(button('Add target', addTarget), submit);
    form.addEventListener('submit', async event => {
      event.preventDefault(); submit.disabled = true;
      try {
        const args = { name: name.value.trim(), path: path.value.trim(), platforms: rows.map(({ fields }) =>
          Object.fromEntries(Object.entries(fields).map(([key, field]) => [key, field.value.trim()]))) };
        if (branches.value.trim()) args.branches = branches.value.split(',').map(value => value.trim()).filter(Boolean);
        const reply = await this.request('registerProject', args);
        this.status(`Registered ${reply.project.name}.`);
        name.value = path.value = branches.value = ''; rows.length = 0; targets.replaceChildren(); addTarget();
        container.open = false; this.projectSignature = ''; await this.refresh();
      } catch (error) { this.status(String(error?.message || error), true); }
      finally { submit.disabled = false; }
    });
    container.append(form); return container;
  }

  block(title, hint, grid = false) {
    const node = element('section', 'dev-block');
    const head = element('div', 'dev-block-head');
    head.append(element('h2', 'small', title));
    if (hint) head.append(element('p', 'hint', hint));
    node.append(head, element('div', grid ? 'service-items dev-grid' : 'service-items'));
    return node;
  }
  items(section) { return section.querySelector('.service-items'); }
  status(text, error = false) { this.note.textContent = text; this.note.className = `status ${error ? 'error' : ''}`; }
  request(operation, args = {}) { return this.invoke('service_request', { operation, arguments: args }); }
  show(visible) {
    this.visible = visible;
    if (visible) this.loadSigning();
    clearInterval(this.timer);
    if (visible && this.connected) { this.refresh(); this.timer = setInterval(() => this.refresh(), 4000); }
  }
  async signIn() {
    this.connect.disabled = true;
    const password = this.password.value;
    this.password.value = '';
    try {
      const service = await this.invoke('service_connect', { url: this.address.value, password });
      this.onConnected(service);
    } catch (error) { this.status(String(error?.message || error), true); }
    finally { this.connect.disabled = false; }
  }
  // The Developer tab now talks to `service`: this Mac's own (local) or another Mac's.
  onConnected(service) {
    {
      this.connected = true;
      this.local = !!service.local;
      this.hostCard.hidden = this.local;
      this.remoteForm.hidden = this.local;
      this.capabilities = new Set(service.capabilities || []);
      this.registration.hidden = !this.capabilities.has('project-registration');
      this.epoch += 1;
      this.fields.hidden = this.hint.hidden = this.intro.hidden = true;
      this.connected_bar.hidden = false;
      this.connected_label.textContent = this.local ? 'The build service is running on this Mac' : `Connected to Apparition Store at ${service.url}`;
      this.disconnect.textContent = this.local ? 'Stop' : 'Disconnect';
      // The remote form holds this bar, so it stays visible (with only the bar) when local.
      this.remoteForm.hidden = false;
      this.fields.hidden = this.hint.hidden = true;
      this.webUi.hidden = !this.local;
      if (this.local) this.loadWebUi();
      this.content.hidden = false;
      // Signing hand-off moves into its section once there is one to put it in.
      this.panes.setup.append(this.webUi, this.signing, this.about);
      this.projectSignature = '';
      this.signatures = {};
      this.status('');
      this.show(true);
    }
  }
  async signOut() {
    clearInterval(this.timer);
    this.connected = false;
    this.epoch += 1;
    this.content.hidden = true;
    this.fields.hidden = this.hint.hidden = this.intro.hidden = false;
    this.connected_bar.hidden = true;
    this.root.append(this.signing);
    this.viewing = null; this.selectedLog = null; this.last = null; this.signatures = {}; this.hosts.clear();
    this.viewer.remove?.(); this.viewer.hidden = true;
    this.resign.hidden = true; this.resignTarget = null;
    this.webUi.hidden = true; this.remoteForm.hidden = true; this.hostCard.hidden = false;
    if (this.local) {
      this.local = false;
      await this.invoke('host_stop').catch(() => {});
      this.status('The build service on this Mac is stopped.');
      return this.hostCheck(false);
    }
    try { await this.invoke('service_disconnect'); this.status('Disconnected.'); }
    catch (error) { this.status(`Disconnected locally. ${error?.message || error}`, true); }
    this.hostCheck(false);
  }

  // This Mac's own build service (desktop/src/host.rs): the app runs it, so there is nothing to
  // install, start or sign in to. Another Mac's service is the alternative.
  hostSection() {
    const card = element('section', 'dev-card dev-host');
    card.append(element('h2', 'small', 'Build service on this Mac'));
    this.hostText = element('p', '', 'Apparition runs Apparition Store while it is open. It builds your Xcode projects and keeps each artifact. Device-side installs are signed by Apparition on the device; direct Mac installs use this Mac’s development certificate and profile.');
    this.hostProblem = element('p', 'status error'); this.hostProblem.hidden = true;
    const row = element('div', 'service-row');
    this.hostStart = button('Start', () => this.startLocal(), 'button primary');
    this.hostRemote = button('Connect to another Mac', () => { this.remoteForm.hidden = false; this.intro.hidden = false; card.hidden = true; }, 'button quiet');
    row.append(this.hostStart, this.hostRemote);
    card.append(this.hostText, this.hostProblem, row);
    return card;
  }
  // On a Mac that carries the service, start it as soon as the tab exists (`auto`); elsewhere
  // only another Mac's service is offered.
  async hostCheck(auto) {
    let status;
    try { status = (await this.invoke('host_status')) || { available: false }; } catch { status = { available: false }; }
    const usable = status.available && status.bundled;
    this.hostCard.hidden = !usable || this.connected;
    this.remoteForm.hidden = !!usable || this.connected;
    this.intro.hidden = !!usable || this.connected;
    if (!usable) return;
    this.hostProblem.hidden = !status.error && !!status.node;
    this.hostProblem.textContent = !status.node ? 'The build service needs Node.js 18 or later on this Mac: brew install node, or nodejs.org.' : status.error || '';
    if (auto && !this.connected && status.node) this.startLocal();
  }
  async startLocal() {
    this.hostStart.disabled = true;
    this.status('Starting the build service…');
    try { this.onConnected(await this.invoke('host_connect')); }
    catch (error) {
      this.hostProblem.hidden = false; this.hostProblem.textContent = String(error?.message || error);
      this.status('');
    } finally { this.hostStart.disabled = false; }
  }
  // Off by default: the web pages, for a browser on another device (a phone tuning an app, say).
  // The same password lets another device's Apparition connect.
  webUiSection() {
    const card = element('section', 'dev-card dev-signing'); card.hidden = true;
    card.append(element('h2', 'small', 'Web UI and other devices'));
    card.append(element('p', '', 'Turn on the web pages to use the build service from a browser, such as a phone beside a headset. The password also lets Apparition on another device connect. The service listens on this Mac only (127.0.0.1); reach it from elsewhere through something like tailscale serve.'));
    const row = element('div', 'dev-webui-row');
    const toggle = element('label', 'check'); this.webUiToggle = element('input'); this.webUiToggle.type = 'checkbox';
    toggle.append(this.webUiToggle, document.createTextNode(' Web UI on'));
    const passwordLabel = element('label', '', 'Password');
    this.webUiPassword = element('input'); this.webUiPassword.type = 'password'; this.webUiPassword.autocomplete = 'new-password';
    this.webUiPassword.placeholder = 'unchanged';
    passwordLabel.append(this.webUiPassword);
    const save = button('Save', () => this.saveWebUi(), 'button primary');
    row.append(toggle, passwordLabel, save);
    this.webUiNote = element('p', 'hint');
    card.append(row, this.webUiNote);
    return card;
  }
  showWebUi(settings) {
    this.webUiToggle.checked = !!settings.webUi;
    this.webUiPassword.placeholder = settings.passwordSet ? 'unchanged' : 'at least 8 characters';
    this.webUiNote.textContent = settings.webUiOn ? `On at http://127.0.0.1:${settings.port}` : settings.passwordSet ? 'Off. A password is set.' : 'Off. No password is set.';
  }
  async loadWebUi() {
    try { this.showWebUi(await this.invoke('host_settings')); } catch (error) { this.webUiNote.textContent = String(error?.message || error); }
  }
  async saveWebUi() {
    const password = this.webUiPassword.value; this.webUiPassword.value = '';
    try { this.showWebUi(await this.invoke('host_web_ui', { enabled: this.webUiToggle.checked, password: password || null })); this.status('Saved.'); }
    catch (error) { this.status(String(error?.message || error), true); }
  }
  async action(operation, args) {
    try { const result = await this.request(operation, args); await this.refresh(); return result; }
    catch (error) { this.status(String(error?.message || error), true); }
  }
  async refresh() {
    if (!this.connected || !this.visible || this.polling) return;
    this.polling = true;
    const epoch = this.epoch;
    try {
      const state = await this.request('state');
      if (!this.connected || !this.visible || epoch !== this.epoch) return;
      this.installs = state.installs || {};
      this.deviceList = state.devices?.list || [];
      this.jobsList = state.builds || [];
      const results = await Promise.allSettled([this.request('traces'), this.request('crashlogs'), this.request('debugApps'),
        this.capabilities.has('artifact-manifests') ? this.request('artifacts') : Promise.resolve({ builds: [] })]);
      if (!this.connected || !this.visible || epoch !== this.epoch) return;
      this.artifactList = results[3].status === 'fulfilled' ? results[3].value.builds || [] : [];
      this.reportCounts = this.countReports(results[0], results[1]);
      this.last = { repos: state.repos || [], traces: results[0], crashes: results[1], debug: results[2], artifacts: results[3] };
      this.renderAll();
      // A log being followed keeps loading until its build is over; everything else opened stays as read.
      if (this.viewing?.live && !this.viewing.done) await this.loadViewing();
    } catch (error) { this.status(String(error?.message || error), true); }
    finally { this.polling = false; }
  }

  countReports(traces, crashes) {
    const counts = {};
    const add = (bundleId, key) => { if (!bundleId) return; counts[bundleId] = counts[bundleId] || { crashes: 0, traces: 0 }; counts[bundleId][key] += 1; };
    if (traces.status === 'fulfilled') for (const trace of traces.value.traces || []) add(trace.app?.bundleId, 'traces');
    if (crashes.status === 'fulfilled') for (const crash of crashes.value.crashLogs || []) add(crash.bundleId, 'crashes');
    return counts;
  }

  // One tile-card per project, two to a row. A card's controls are rebuilt only when what it
  // shows changes, and what the person set on it (branch, options, kept builds opened) is
  // remembered, so a choice in progress survives the 4-second refresh.
  renderProjects(repos) {
    this.repos = repos;
    const signature = JSON.stringify([repos.map(r => [r.name, r.branches, r.platforms, r.deployDefaults, r.git]),
      this.installs, this.artifactList.map(a => a.buildId), this.jobsList.map(j => [j.id, j.status]), this.reportCounts,
      this.deviceList?.map(d => [d.name, d.state]), [...this.capabilities], this.viewKey(),
      repos.flatMap(r => (r.platforms || []).map(t => targetState(r, t, this.installs).text))]);
    if (signature === this.projectSignature) return;
    this.projectSignature = signature;
    this.cardState = this.cardState || {};
    const rows = repos.map(repo => this.projectCard(repo));
    this.items(this.projects).replaceChildren(...(rows.length ? rows : [element('p', 'hint',
      this.capabilities.has('project-registration') ? 'No projects yet. Register one below, or add it to Apparition Store\'s config.json.' : 'Add a project to Apparition Store\'s config.json to start building.')]));
    this.placeViewer();
  }
  projectCard(repo) {
    const remembered = this.cardState[repo.name] = this.cardState[repo.name] || {};
    const card = element('article', 'dev-project');
    const head = element('div', 'dev-project-head');
    const title = element('div', 'dev-project-title');
    title.append(element('h3', '', repo.name));
    if (repo.git?.commit) title.append(pill(`${repo.git.branch}@${repo.git.commit}`, 'mono', 'The branch and commit checked out on the Mac'));
    if (repo.git?.dirty) title.append(pill('Uncommitted changes', 'warn', 'The checkout has changes that are not committed; a build includes them.'));
    head.append(title);
    card.append(head);

    // Build options apply to every action on this card.
    const options = element('details', 'dev-options');
    options.open = !!remembered.open;
    options.addEventListener('toggle', () => { remembered.open = options.open; });
    options.append(element('summary', '', 'Build options'));
    const branch = element('select'); branch.setAttribute('aria-label', `${repo.name} branch`);
    for (const name of ['current', ...(repo.branches || []).filter(b => b !== 'current')]) {
      const option = element('option', '', name === 'current' ? 'Current checkout' : name); option.value = name; branch.append(option);
    }
    if (remembered.branch) branch.value = remembered.branch;
    branch.addEventListener('change', () => { remembered.branch = branch.value; });
    const optionRow = element('div', 'service-row');
    const branchLabel = element('label', 'dev-inline', 'Branch'); branchLabel.append(branch); optionRow.append(branchLabel);
    const flags = {};
    for (const [key, title] of [['run', 'Launch after installing'], ['mcp', 'Debug access'], ['inject', 'Live patching'], ['buildDebug', 'Debug configuration']]) {
      const label = element('label', 'check'); const input = element('input'); input.type = 'checkbox';
      input.checked = remembered[key] ?? !!repo.deployDefaults?.[key === 'run' ? 'launch' : key]; flags[key] = input;
      input.addEventListener('change', () => { remembered[key] = input.checked; });
      label.append(input, document.createTextNode(title)); optionRow.append(label);
    }
    if (this.capabilities.has('entry-patching')) {
    const backendLabel = element('label', 'dev-inline', 'Patch backend');
    const backend = element('select'); backend.setAttribute('aria-label', `${repo.name} patch backend`);
    for (const [value, title] of [['dynamic', 'Swift dynamic replacement'], ['apparition', 'Apparition entry boundaries (experimental)']]) {
      const option = element('option', '', title); option.value = value; backend.append(option);
    }
    backend.value = remembered.injectBackend || 'dynamic';
    backend.addEventListener('change', () => { remembered.injectBackend = backend.value; });
    flags.injectBackend = backend;
    backendLabel.append(backend); optionRow.append(backendLabel);
    }
    options.append(optionRow);
    this.projectControls = this.projectControls || {};
    this.projectControls[repo.name] = { branch, flags };

    const targets = element('div', 'dev-targets');
    for (const target of repo.platforms || []) targets.append(this.targetRow(repo, target, { branch, flags }));
    if (!(repo.platforms || []).length) targets.append(element('p', 'hint', 'No targets. Add one to the project in Apparition Store\'s config.json.'));
    card.append(targets, options);

    const footer = element('div', 'dev-project-foot');
    const rowId = `project:${repo.name}`;
    const latest = this.jobsList.find(job => job.repo === repo.name);
    if (latest) {
      footer.append(pill(`${jobKind(latest)} #${latest.id} · ${latest.status}`, statusTone(latest.status)));
      footer.append(this.viewButton(rowId, `log:${latest.id}`, 'Log', 'Hide log',
        { title: `Build #${latest.id}`, operation: 'buildLog', args: { id: latest.id }, live: true }));
    }
    const bundleIds = new Set((repo.platforms || []).map(t => t.bundleId).filter(Boolean));
    const reports = [...bundleIds].reduce((sum, id) => {
      const c = this.reportCounts?.[id]; return c ? { crashes: sum.crashes + c.crashes, traces: sum.traces + c.traces } : sum;
    }, { crashes: 0, traces: 0 });
    if (reports.crashes || reports.traces) footer.append(button(`${plural(reports.crashes, 'crash')} · ${plural(reports.traces, 'trace')}`, () => this.select('reports'), 'button quiet small'));
    const kept = this.artifactList.filter(a => bundleIds.has(a.app?.bundleId) || bundleIds.has(a.app?.originalBundleId));
    if (kept.length) {
      const list = element('details', 'dev-kept');
      list.open = !!remembered.keptOpen;
      list.addEventListener('toggle', () => { remembered.keptOpen = list.open; });
      list.append(element('summary', '', `${plural(kept.length, 'kept build')}`));
      for (const build of kept.slice(0, 12)) list.append(this.artifactRow(build, repo.name));
      footer.append(list);
    }
    if (footer.children.length) card.append(footer);
    this.host(rowId, card);
    return card;
  }
  // A target: where it installs, what is there now, and the three things to do about it.
  targetRow(repo, target, { branch, flags }) {
    const row = element('div', 'dev-target');
    const where = element('div', 'dev-target-where');
    const device = (this.deviceList || []).find(d => d.name === target.device);
    const reachable = device?.state === 'connected';
    where.append(element('span', `dot ${reachable ? 'on' : ''}`));
    where.append(element('strong', '', `${target.device}`));
    where.append(pill(target.platform || '', 'mono'));
    where.append(device ? pill(reachable ? 'Reachable' : 'Not reachable now', reachable ? 'good' : '')
      : pill('Not paired with this Mac', 'warn', 'Pair it in Xcode (Window > Devices and Simulators) to install from this Mac.'));
    const badge = targetBadge(repo, target, this.installs);
    where.append(pill(badge.text, badge.tone));
    const { install, text } = targetState(repo, target, this.installs);
    const state = element('p', 'hint dev-target-state', text);
    const actions = element('div', 'service-row');
    const submit = async (deploy, control) => {
      control.disabled = true;
      try {
        const reply = await this.action('build', { repo: repo.name, device: target.device, targetPlatform: target.platform, branch: branch.value,
          ...Object.fromEntries(Object.entries(flags).map(([key, input]) => [key, key === 'injectBackend' ? input.value : (deploy ? input.checked : (key === 'run' ? false : input.checked))])),
          ...(deploy ? {} : { deploy: false }) });
        if (reply?.jobs?.[0]) { this.followJob(reply.jobs[0].id); this.status(`Queued ${deploy ? 'build' : 'build-only'} #${this.selectedLog}. Follow it in Activity.`); }
      } finally { control.disabled = false; }
    };
    if (this.capabilities.has('entry-patching')) {
      const patchButton = button('Patch running app', async () => {
        patchButton.disabled = true;
        try {
          const reply = await this.action('inject', { repo: repo.name, device: target.device, targetPlatform: target.platform });
          this.status(JSON.stringify(reply));
        } finally { patchButton.disabled = false; }
      }, 'button quiet small');
      actions.append(patchButton);
    }
    const installButton = button(`Install on ${target.device}`, () => submit(true, installButton), 'button primary small');
    const buildOnly = button('Build only', () => submit(false, buildOnly), 'button quiet small');
    buildOnly.title = 'Builds and keeps the IPA without installing. Download it to open in Apparition, or use it for a re-sign.';
    actions.append(installButton, buildOnly);
    if (this.capabilities.has('resign') && !/^macos$/i.test(target.platform)) {
      // The exact build on the device if the Mac kept it; otherwise build it first, if the Mac can.
      const keptInstalled = install?.buildId && this.artifactList.find(a => a.buildId === install.buildId);
      const source = keptInstalled
        ? { buildId: keptInstalled.buildId, label: keptInstalled.app?.build || keptInstalled.buildId }
        : this.capabilities.has('resign-build') ? { repo: repo.name } : null;
      if (source) actions.append(button('Re-sign…', () => this.openResign({ ...source, repo: repo.name, device: target.device,
        bundleId: target.bundleId, platform: target.platform, name: repo.name }), 'button quiet small'));
    }
    row.append(where, state, actions);
    return row;
  }

  renderDevices(list) {
    this.paint('devices', JSON.stringify(list.map(d => [d.name, d.model, d.platform, d.state])), () => {
      const cards = list.map(device => {
        const on = device.state === 'connected';
        const card = element('div', 'dev-device');
        const head = element('div', 'dev-device-head');
        const state = on ? 'Reachable' : String(device.state || 'unknown').replace(/^./, c => c.toUpperCase());
        head.append(element('span', `dot ${on ? 'on' : ''}`), element('strong', '', device.name), pill(state, on ? 'good' : ''));
        card.append(head, element('span', 'hint', [device.model, device.platform].filter(Boolean).join(' · ')));
        return card;
      });
      this.items(this.devices).replaceChildren(...(cards.length ? cards : [element('p', 'hint', 'No devices yet. Pair one with this Mac in Xcode (Window > Devices and Simulators).')]));
    });
  }
  renderJobs(jobs) {
    const active = jobs.filter(job => ACTIVE.includes(job.status));
    const finished = jobs.filter(job => !ACTIVE.includes(job.status));
    const shown = this.showAllJobs ? finished : finished.slice(0, 8);
    if (this.navButtons) this.navButtons.activity.textContent = active.length ? `Activity · ${active.length} running` : 'Activity';
    this.paint('jobs', JSON.stringify([jobs, this.showAllJobs, this.viewKey(), Math.floor(Date.now() / 60000)]), () => {
      const rows = [...active, ...shown].map(job => this.jobRow(job));
      if (finished.length > shown.length) rows.push(button(`Show ${finished.length - shown.length} older`, () => { this.showAllJobs = true; this.renderJobs(this.jobsList); }));
      this.items(this.jobs).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No builds yet. Start one from a project.')]));
    });
  }
  // A job: its state, what it built, and its log, opening right under it.
  jobRow(job) {
    const rowId = `job:${job.id}`;
    const row = element('div', 'dev-job');
    const main = element('div', 'dev-job-main');
    const body = element('div', 'dev-job-body');
    body.append(element('strong', '', `${jobKind(job)} #${job.id}`));
    body.append(element('span', 'hint', `${job.repo} · ${job.platform}${job.device ? ` · ${job.device}` : ''}${job.commit ? ` · ${job.builtBranch || ''}@${job.commit}` : ''}`));
    if (job.error) body.append(element('span', 'dev-job-error', job.error));
    const actions = element('div', 'dev-job-actions');
    actions.append(this.viewButton(rowId, 'log', 'Log', 'Hide log', { title: `Build #${job.id}`, operation: 'buildLog', args: { id: job.id }, live: true }));
    if (job.buildId && job.artifact) {
      actions.append(this.viewButton(rowId, 'manifest', 'Build details', 'Hide details',
        { title: `Build ${job.buildId}`, operation: 'artifactManifest', args: { id: job.buildId } }));
      actions.append(this.downloadButton(job.buildId));
    }
    if (ACTIVE.includes(job.status)) actions.append(button('Cancel', () => this.action('cancel', { id: job.id }), 'button quiet small'));
    main.append(pill(job.status, `jobstate ${statusTone(job.status)}`), body, actions);
    row.append(main);
    this.host(rowId, row);
    return row;
  }
  downloadButton(id) {
    const download = button('Download', async () => {
      download.disabled = true; download.textContent = 'Downloading…';
      try {
        const result = await this.invoke('service_download', { id });
        if (!result.cancelled) this.status(`Saved verified artifact to ${result.path}`);
      } catch (error) { this.status(String(error?.message || error), true); }
      finally { download.disabled = false; download.textContent = 'Download'; }
    }, 'button quiet small');
    return download;
  }
  // A kept build; `where` keeps its inline viewer apart when it is listed in two places.
  artifactRow(build, where = 'kept') {
    const rowId = `artifact:${where}:${build.buildId}`;
    const row = element('div', 'dev-item');
    const main = element('div', 'dev-item-main');
    main.append(element('span', 'dev-item-text', `${build.app?.name || build.app?.bundleId || 'App'} · ${build.platform || ''} · ${build.app?.build || build.buildId}${build.createdAt ? ` · ${ago(Date.parse(build.createdAt))}` : ''}`));
    const actions = element('div', 'dev-job-actions');
    actions.append(this.viewButton(rowId, 'manifest', 'Build details', 'Hide details',
      { title: `Build ${build.buildId}`, operation: 'artifactManifest', args: { id: build.buildId } }));
    actions.append(this.downloadButton(build.buildId));
    const resign = this.resignButton({ buildId: build.buildId, bundleId: build.app?.bundleId, platform: build.platform, name: build.app?.name, label: build.app?.build });
    if (resign) { resign.className = 'button quiet small'; actions.append(resign); }
    main.append(actions);
    row.append(main);
    this.host(rowId, row);
    return row;
  }
  // Kept builds no project claims; a project's own are listed on its card.
  renderArtifacts(result, jobs) {
    const builds = result.status === 'fulfilled' ? result.value.builds || [] : [];
    this.paint('artifacts', JSON.stringify([result.status, builds.map(b => b.buildId), jobs.map(j => j.buildId), this.repos?.map(r => r.platforms),
      this.viewKey(), [...this.capabilities], Math.floor(Date.now() / 60000)]), () => {
      const rows = [];
      if (result.status === 'fulfilled') {
        const claimed = new Set((this.repos || []).flatMap(repo => (repo.platforms || []).map(t => t.bundleId)).filter(Boolean));
        const inJobs = new Set(jobs.map(job => job.buildId).filter(Boolean));
        for (const build of builds) {
          if (inJobs.has(build.buildId) || claimed.has(build.app?.bundleId) || claimed.has(build.app?.originalBundleId)) continue;
          rows.push(this.artifactRow(build, 'other'));
        }
      } else rows.push(element('p', 'error', 'Kept builds could not be loaded.'));
      this.artifacts.hidden = !rows.length || !this.capabilities.has('artifact-manifests');
      this.items(this.artifacts).replaceChildren(...rows);
    });
  }
  renderReports(traces, crashes) {
    const groups = new Map();
    const group = (bundleId, name) => {
      const key = bundleId || name || 'Unknown app';
      if (!groups.has(key)) groups.set(key, { key, name: name || bundleId || 'Unknown app', bundleId, rows: [], crashes: 0, traces: 0 });
      return groups.get(key);
    };
    const errors = [];
    const sources = [traces, crashes].map(r => r.status === 'fulfilled' ? r.value : null);
    const signature = JSON.stringify([sources, traces.status, crashes.status, this.viewKey(), [...this.expandedGroups], Math.floor(Date.now() / 60000)]);
    // The tab's count is part of the page chrome, so it follows the data even when the list is unchanged.
    const total = (sources[0]?.traces?.length || 0) + (sources[1]?.crashLogs?.length || 0);
    if (this.navButtons) this.navButtons.reports.textContent = total ? `Reports · ${total}` : 'Reports';
    this.paint('reports', signature, () => {
      if (crashes.status === 'fulfilled') {
        const sorted = [...(crashes.value.crashLogs || [])].sort((a, b) => (b.capturedAt || 0) - (a.capturedAt || 0));
        for (const crash of sorted) {
          const g = group(crash.bundleId, crash.appName); g.crashes += 1;
          const rowId = `crash:${crash.id}`;
          const row = element('div', 'dev-report');
          const main = element('div', 'dev-item-main');
          const body = element('div', 'dev-job-body');
          body.append(element('strong', '', crash.exception || 'Crash report'));
          body.append(element('span', 'hint', [crash.device, crash.build ? `build ${crash.build}` : '', crash.capturedAt ? ago(crash.capturedAt) : ''].filter(Boolean).join(' · ')));
          const actions = element('div', 'dev-job-actions');
          actions.append(this.viewButton(rowId, 'crash', crash.symbolicated ? 'Read symbolicated' : 'Read crash', 'Hide report',
            { title: `${crash.exception || 'Crash report'} · ${crash.device || ''}`.trim(), operation: 'crashFile', args: { id: crash.id, raw: !crash.symbolicated } }));
          main.append(pill('Crash', 'bad'), body, actions);
          row.append(main); this.host(rowId, row);
          g.rows.push(row);
        }
      } else errors.push(element('p', 'error', 'Crash reports could not be loaded.'));
      if (traces.status === 'fulfilled') {
        for (const trace of traces.value.traces || []) {
          const g = group(trace.app?.bundleId, trace.app?.name); g.traces += 1;
          const rowId = `trace:${trace.id}`;
          const row = element('div', 'dev-report');
          const main = element('div', 'dev-item-main');
          const body = element('div', 'dev-job-body');
          body.append(element('strong', '', trace.note || 'Debug trace'));
          if (trace.app?.build) body.append(element('span', 'hint', `build ${trace.app.build}`));
          const actions = element('div', 'dev-job-actions');
          if (trace.build?.buildId) actions.append(this.viewButton(rowId, 'manifest', 'Build details', 'Hide details',
            { title: `Build ${trace.build.buildId}`, operation: 'artifactManifest', args: { id: trace.build.buildId } }));
          for (const file of trace.files || []) actions.append(this.viewButton(rowId, `file:${file.path}`, file.path, `Hide ${file.path}`,
            { title: file.path, operation: 'traceFile', args: { id: trace.id, path: file.path } }));
          main.append(pill('Trace', 'info'), body, actions);
          row.append(main); this.host(rowId, row);
          g.rows.push(row);
        }
      } else errors.push(element('p', 'error', 'Debug traces could not be loaded.'));
      const LIMIT = 8;
      const rows = [...groups.values()].sort((a, b) => a.name.localeCompare(b.name)).map(g => {
        const details = element('details', 'dev-group');
        details.open = this.openGroups.has(g.key);
        details.addEventListener('toggle', () => { if (details.open) this.openGroups.add(g.key); else this.openGroups.delete(g.key); });
        const summary = element('summary');
        summary.append(element('span', 'dev-group-name', g.name));
        if (g.crashes) summary.append(pill(plural(g.crashes, 'crash'), 'bad'));
        if (g.traces) summary.append(pill(plural(g.traces, 'trace'), 'info'));
        const body = element('div', 'dev-group-body');
        const expanded = this.expandedGroups.has(g.key);
        body.append(...(expanded ? g.rows : g.rows.slice(0, LIMIT)));
        if (g.rows.length > LIMIT && !expanded) {
          body.append(button(`Show ${g.rows.length - LIMIT} more`, () => { this.expandedGroups.add(g.key); this.openGroups.add(g.key); this.redraw(); }, 'button quiet small'));
        }
        details.append(summary, body);
        return details;
      });
      this.items(this.reports).replaceChildren(...errors, ...(rows.length ? rows : [element('p', 'hint', 'No reports yet.')]));
    });
  }
  renderDebug(result) {
    const apps = result.status === 'fulfilled' ? result.value : null;
    this.paint('debug', JSON.stringify([result.status, apps, this.viewKey()]), () => {
      const rows = [];
      if (result.status === 'fulfilled') {
        for (const app of result.value.apps || []) {
          const rowId = `debug:${app.id || app.bundleId || app.appName}`;
          const row = element('div', 'dev-item');
          const main = element('div', 'dev-item-main');
          main.append(element('span', 'dot on'), element('span', 'dev-item-text', `${app.appName || app.bundleId} · ${app.device || ''}`));
          const actions = element('div', 'dev-job-actions');
          const inspect = this.viewButton(rowId, 'info', 'Inspect', 'Hide',
            { title: app.appName || app.bundleId, operation: 'debugCall', args: { id: app.id, endpoint: '_info', arguments: {} } });
          inspect.disabled = !app.attachable;
          actions.append(inspect);
          // This Mac's service: its Debug console (live log, tuning panels, profile, memory) in a window.
          if (this.local && app.id) {
            const consoleButton = button('Console', () => this.invoke('host_open_console', { key: app.id })
              .catch(error => this.status(String(error?.message || error), true)), 'button quiet small');
            consoleButton.disabled = !app.attachable;
            actions.append(consoleButton);
          }
          main.append(actions);
          row.append(main); this.host(rowId, row);
          rows.push(row);
        }
        if (result.value.error) rows.push(element('p', 'hint', String(result.value.error)));
      } else rows.push(element('p', 'error', 'Running apps could not be loaded.'));
      this.items(this.debug).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No debug apps running.')]));
    });
  }
}
