// The build service is reached only through Rust. No cookies, bearer tokens or
// remote HTML enter this view; all service-provided strings become text nodes.
//
// Layout: before connecting, a getting-started page (what appstore is, where to get it, how
// to connect). Once connected, four sections: Projects (one card per project, one row per
// target with what is installed there and what can be done), Activity (the build queue and
// logs), Reports (crashes, traces and running debug apps, grouped by app) and Devices &
// signing.
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

export class DeveloperView {
  constructor(root, invoke) {
    this.root = root;
    this.invoke = invoke;
    this.connected = false;
    this.visible = false;
    this.polling = false;
    this.selectedLog = null;
    this.projectSignature = '';
    this.epoch = 0;
    this.capabilities = new Set();
    this.repos = [];
    this.artifactList = [];
    this.installs = {};
    this.jobsList = [];
    this.section = 'projects';
    this.showAllJobs = false;
    root.append(element('h1', '', 'Developer'));
    root.append(element('p', 'lede', 'Build your own Xcode projects on this Mac, install them on your devices, and bring their crash logs and traces back to the project.'));
    this.intro = this.introCard();
    const form = element('form', 'dev-card dev-connect');
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
    this.hint = element('p', 'hint', 'The password is the one set with node server.js set-password in appstore. On this Mac, http://localhost:8484 works; elsewhere use its HTTPS address. Apparition forgets the session when it quits.');
    this.fields = fields;
    // Once connected the form folds into one line: where, and a way out.
    this.connected_bar = element('div', 'dev-connected'); this.connected_bar.hidden = true;
    this.connected_label = element('span', 'dev-connected-label selectable');
    this.disconnect = button('Disconnect', () => this.signOut());
    this.connected_bar.append(element('span', 'dot'), this.connected_label, this.disconnect);
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
    this.projects = this.block('Projects', 'Each target is a device the project installs on. Install builds and installs it there; Build only keeps a signed IPA to download, install over the air or re-sign.');
    this.registration = this.registrationForm(); this.registration.hidden = true;
    this.artifacts = this.block('Other kept builds', 'Signed builds the Mac kept that no project here installs, such as Xcode products registered by hand.');
    this.artifacts.hidden = true;
    this.panes.projects.append(this.resign, this.projects, this.registration, this.artifacts);

    this.jobs = this.block('Builds', 'Newest first. Builds for one project run one at a time.');
    this.panes.activity.append(this.jobs);

    this.reports = this.block('Crash logs and traces', 'Pulled from your devices and uploaded by apps that link DebugTrace, grouped by app.');
    this.debug = this.block('Running apps', 'Dev builds with a debug server that are running now. Inspecting one asks for approval on the device.');
    this.panes.reports.append(this.reports, this.debug);

    this.devices = this.block('Devices', 'Devices this Mac can install to. Simulators are not listed.');
    this.panes.setup.append(this.devices);

    this.detail = element('pre', 'service-detail selectable');
    this.detail.tabIndex = 0; this.detail.hidden = true;
    this.content.append(this.nav, ...SECTIONS.map(([key]) => this.panes[key]), this.detail);
    root.append(this.intro, form, this.note, this.content, this.signing);
    this.select('projects');
    invoke('service_settings').then(settings => { if (settings.url) this.address.value = settings.url; }).catch(() => {});
  }

  // What this is and how to start, for someone who has never run appstore.
  introCard() {
    const card = element('section', 'dev-card dev-intro');
    card.append(element('h2', 'small', 'Get started'));
    card.append(element('p', '', 'The Developer tools work with appstore, an open-source build service you run on your own Mac. It builds your projects with its build-and-sign script and your developer certificate, installs them on devices paired with the Mac, keeps every signed build, and collects crash logs and debug traces. Apparition connects to it from this Mac or from a headset, iPhone or Apple TV.'));
    const steps = element('ol', 'dev-steps');
    for (const text of [
      'Get appstore from GitHub and follow its README. It needs Xcode, Node 18 and a paid developer certificate with a development profile.',
      'Set a service password: node server.js set-password.',
      'Start it (node server.js, or as a launch agent), then connect below. Register your projects here once connected.',
    ]) steps.append(element('li', '', text));
    const links = element('div', 'service-row');
    links.append(button('appstore on GitHub', () => this.open('appstore')), button('Developer guide', () => this.open('guide')));
    card.append(steps, links);
    return card;
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
        this.selectedLog = job.id;
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
    form.append(element('p', 'hint', 'Choose an existing Git checkout with build-signing.conf already set up (see appstore\'s README). Registration saves its targets; it does not build or install anything.'));
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

  block(title, hint) {
    const node = element('section', 'dev-block');
    const head = element('div', 'dev-block-head');
    head.append(element('h2', 'small', title));
    if (hint) head.append(element('p', 'hint', hint));
    node.append(head, element('div', 'service-items'));
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
      this.connected = true;
      this.capabilities = new Set(service.capabilities || []);
      this.registration.hidden = !this.capabilities.has('project-registration');
      this.epoch += 1;
      this.fields.hidden = this.hint.hidden = this.intro.hidden = true;
      this.connected_bar.hidden = false; this.connected_label.textContent = `Connected to ${service.url}`;
      this.content.hidden = false;
      // Signing hand-off moves into its section once there is one to put it in.
      this.panes.setup.append(this.signing);
      this.projectSignature = '';
      this.status('');
      this.show(true);
    } catch (error) { this.status(String(error?.message || error), true); }
    finally { this.connect.disabled = false; }
  }
  async signOut() {
    clearInterval(this.timer);
    this.connected = false;
    this.epoch += 1;
    this.content.hidden = true;
    this.fields.hidden = this.hint.hidden = this.intro.hidden = false;
    this.connected_bar.hidden = true;
    this.root.append(this.signing);
    this.detail.replaceChildren(); this.detail.hidden = true; this.resign.hidden = true; this.resignTarget = null;
    try { await this.invoke('service_disconnect'); this.status('Disconnected.'); }
    catch (error) { this.status(`Disconnected locally. ${error?.message || error}`, true); }
  }
  async action(operation, args) {
    try { const result = await this.request(operation, args); await this.refresh(); return result; }
    catch (error) { this.status(String(error?.message || error), true); }
  }
  showDetail(title, value) {
    this.detail.textContent = title + '\n\n' + (typeof value === 'string' ? value : JSON.stringify(value, null, 2));
    this.detail.hidden = false;
  }
  async read(operation, args, title) {
    const epoch = this.epoch;
    try {
      const reply = await this.request(operation, args);
      if (epoch !== this.epoch || !this.connected) return;
      this.showDetail(title, reply.text ?? reply);
    } catch (error) { this.status(String(error?.message || error), true); }
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
      this.projectSignature = '';
      this.renderProjects(state.repos || []);
      this.renderDevices(this.deviceList);
      this.renderJobs(this.jobsList);
      this.renderReports(results[0], results[1]);
      this.renderDebug(results[2]);
      this.renderArtifacts(results[3], this.jobsList);
      const running = this.jobsList.filter(job => ACTIVE.includes(job.status)).length;
      this.navButtons.activity.textContent = running ? `Activity · ${running} running` : 'Activity';
      if (this.selectedLog) {
        const logId = this.selectedLog;
        const log = await this.request('buildLog', { id: logId });
        if (this.selectedLog === logId && this.connected && epoch === this.epoch) this.showDetail(`Build #${logId} · ${log.status}`, log.chunk);
      }
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

  // One card per project. The card's controls are rebuilt only when the project's shape changes,
  // so a choice in progress survives the 4-second refresh; the target rows refresh every time.
  renderProjects(repos) {
    this.repos = repos;
    const signature = JSON.stringify([repos.map(r => [r.name, r.branches, r.platforms, r.deployDefaults, r.git]),
      this.installs, this.artifactList.map(a => a.buildId), this.jobsList.map(j => [j.id, j.status]), this.reportCounts,
      this.deviceList?.map(d => [d.name, d.state]), [...this.capabilities]]);
    if (signature === this.projectSignature) return;
    this.projectSignature = signature;
    this.cardState = this.cardState || {};
    const rows = repos.map(repo => this.projectCard(repo));
    this.items(this.projects).replaceChildren(...(rows.length ? rows : [element('p', 'hint',
      this.capabilities.has('project-registration') ? 'No projects yet. Register one below, or add it to appstore\'s config.json.' : 'Add a project to appstore\'s config.json to start building.')]));
  }
  projectCard(repo) {
    const remembered = this.cardState[repo.name] = this.cardState[repo.name] || {};
    const card = element('article', 'dev-project');
    const head = element('div', 'dev-project-head');
    head.append(element('h3', '', repo.name));
    if (repo.git?.commit) head.append(element('span', 'badge', `${repo.git.branch}@${repo.git.commit}${repo.git.dirty ? ' · uncommitted changes' : ''}`));
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
    options.append(optionRow);
    this.projectControls = this.projectControls || {};
    this.projectControls[repo.name] = { branch, flags };

    const targets = element('div', 'dev-targets');
    for (const target of repo.platforms || []) targets.append(this.targetRow(repo, target, { branch, flags }));
    if (!(repo.platforms || []).length) targets.append(element('p', 'hint', 'No targets. Add one to the project in appstore\'s config.json.'));
    card.append(targets, options);

    const footer = element('div', 'dev-project-foot');
    const latest = this.jobsList.find(job => job.repo === repo.name);
    if (latest) {
      footer.append(element('span', `dev-job-status ${latest.status}`, `${jobKind(latest)} #${latest.id} · ${latest.status}`));
      footer.append(button('Log', () => { this.selectedLog = latest.id; this.read('buildLog', { id: latest.id }, `Build #${latest.id}`); }));
    }
    const bundleIds = new Set((repo.platforms || []).map(t => t.bundleId).filter(Boolean));
    const reports = [...bundleIds].reduce((sum, id) => {
      const c = this.reportCounts?.[id]; return c ? { crashes: sum.crashes + c.crashes, traces: sum.traces + c.traces } : sum;
    }, { crashes: 0, traces: 0 });
    if (reports.crashes || reports.traces) footer.append(button(`${plural(reports.crashes, 'crash')} · ${plural(reports.traces, 'trace')}`, () => this.select('reports')));
    const kept = this.artifactList.filter(a => bundleIds.has(a.app?.bundleId) || bundleIds.has(a.app?.originalBundleId));
    if (kept.length) {
      const list = element('details', 'dev-kept');
      list.append(element('summary', '', `${plural(kept.length, 'kept build')}`));
      for (const build of kept.slice(0, 12)) list.append(this.artifactRow(build));
      footer.append(list);
    }
    if (footer.children.length) card.append(footer);
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
    where.append(element('span', 'hint', ` · ${target.platform}${device ? (reachable ? '' : ' · not reachable now') : ' · not paired with this Mac'}`));
    const { install, text } = targetState(repo, target, this.installs);
    const state = element('p', 'hint dev-target-state', text);
    const actions = element('div', 'service-row');
    const submit = async (deploy, control) => {
      control.disabled = true;
      try {
        const reply = await this.action('build', { repo: repo.name, device: target.device, targetPlatform: target.platform, branch: branch.value,
          ...Object.fromEntries(Object.entries(flags).map(([key, input]) => [key, deploy ? input.checked : (key === 'run' ? false : input.checked)])),
          ...(deploy ? {} : { deploy: false }) });
        if (reply?.jobs?.[0]) { this.selectedLog = reply.jobs[0].id; this.status(`Queued ${deploy ? 'build' : 'build-only'} #${this.selectedLog}. Follow it in Activity.`); }
      } finally { control.disabled = false; }
    };
    const installButton = button(`Install on ${target.device}`, () => submit(true, installButton), 'button primary');
    const buildOnly = button('Build only', () => submit(false, buildOnly));
    buildOnly.title = 'Builds and signs without installing; the kept IPA can be downloaded, installed over the air or re-signed.';
    actions.append(installButton, buildOnly);
    if (this.capabilities.has('resign') && !/^macos$/i.test(target.platform)) {
      // The exact build on the device if the Mac kept it; otherwise build it first, if the Mac can.
      const keptInstalled = install?.buildId && this.artifactList.find(a => a.buildId === install.buildId);
      const source = keptInstalled
        ? { buildId: keptInstalled.buildId, label: keptInstalled.app?.build || keptInstalled.buildId }
        : this.capabilities.has('resign-build') ? { repo: repo.name } : null;
      if (source) actions.append(button('Re-sign…', () => this.openResign({ ...source, repo: repo.name, device: target.device,
        bundleId: target.bundleId, platform: target.platform, name: repo.name })));
    }
    row.append(where, state, actions);
    return row;
  }

  renderDevices(list) {
    const rows = list.map(device => {
      const row = element('div', 'service-row');
      row.append(element('span', `dot ${device.state === 'connected' ? 'on' : ''}`));
      row.append(element('span', '', `${device.name} · ${device.model || device.platform || ''} · ${device.state === 'connected' ? 'reachable' : device.state || 'unknown'}`));
      return row;
    });
    this.items(this.devices).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No devices yet. Pair one with this Mac in Xcode (Window > Devices and Simulators).')]));
  }
  renderJobs(jobs) {
    const active = jobs.filter(job => ACTIVE.includes(job.status));
    const finished = jobs.filter(job => !ACTIVE.includes(job.status));
    const shown = this.showAllJobs ? finished : finished.slice(0, 8);
    const rows = [...active, ...shown].map(job => this.jobRow(job));
    if (finished.length > shown.length) rows.push(button(`Show ${finished.length - shown.length} older`, () => { this.showAllJobs = true; this.renderJobs(this.jobsList); }));
    this.items(this.jobs).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No builds yet. Start one from a project.')]));
  }
  jobRow(job) {
    const row = element('div', 'service-row dev-job');
    row.append(element('span', `dev-job-status ${job.status}`, job.status));
    const what = `${jobKind(job)} #${job.id} · ${job.repo} · ${job.platform}${job.device ? ` · ${job.device}` : ''}${job.commit ? ` · ${job.builtBranch || ''}@${job.commit}` : ''}`;
    row.append(element('span', '', job.error ? `${what} — ${job.error}` : what));
    row.append(button('Log', async () => { this.selectedLog = job.id; await this.read('buildLog', { id: job.id }, `Build #${job.id}`); }));
    if (job.buildId && job.artifact) {
      row.append(button('Build details', () => { this.selectedLog = null; this.read('artifactManifest', { id: job.buildId }, `Build ${job.buildId}`); }));
      row.append(this.downloadButton(job.buildId));
    }
    if (ACTIVE.includes(job.status)) row.append(button('Cancel', () => this.action('cancel', { id: job.id })));
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
    });
    return download;
  }
  artifactRow(build) {
    const row = element('div', 'service-row');
    row.append(element('span', '', `${build.app?.name || build.app?.bundleId || 'App'} · ${build.platform || ''} · ${build.app?.build || build.buildId}${build.createdAt ? ` · ${ago(Date.parse(build.createdAt))}` : ''}`));
    row.append(button('Build details', () => { this.selectedLog = null; this.read('artifactManifest', { id: build.buildId }, `Build ${build.buildId}`); }));
    row.append(this.downloadButton(build.buildId));
    const resign = this.resignButton({ buildId: build.buildId, bundleId: build.app?.bundleId, platform: build.platform, name: build.app?.name, label: build.app?.build });
    if (resign) row.append(resign);
    return row;
  }
  // Kept builds no project claims; a project's own are listed on its card.
  renderArtifacts(result, jobs) {
    const rows = [];
    if (result.status === 'fulfilled') {
      const claimed = new Set((this.repos || []).flatMap(repo => (repo.platforms || []).map(t => t.bundleId)).filter(Boolean));
      const inJobs = new Set(jobs.map(job => job.buildId).filter(Boolean));
      for (const build of result.value.builds || []) {
        if (inJobs.has(build.buildId) || claimed.has(build.app?.bundleId) || claimed.has(build.app?.originalBundleId)) continue;
        rows.push(this.artifactRow(build));
      }
    } else rows.push(element('p', 'error', 'Kept builds could not be loaded.'));
    this.artifacts.hidden = !rows.length || !this.capabilities.has('artifact-manifests');
    this.items(this.artifacts).replaceChildren(...rows);
  }
  renderReports(traces, crashes) {
    const groups = new Map();
    const group = (bundleId, name) => {
      const key = bundleId || name || 'Unknown app';
      if (!groups.has(key)) groups.set(key, { name: name || bundleId || 'Unknown app', bundleId, rows: [], crashes: 0, traces: 0 });
      return groups.get(key);
    };
    const errors = [];
    if (crashes.status === 'fulfilled') {
      for (const crash of crashes.value.crashLogs || []) {
        const g = group(crash.bundleId, crash.appName); g.crashes += 1;
        const row = element('div', 'service-row');
        row.append(element('span', '', `Crash · ${crash.exception || 'report'} · ${crash.device || ''}${crash.build ? ` · build ${crash.build}` : ''}${crash.capturedAt ? ` · ${ago(crash.capturedAt)}` : ''}`));
        row.append(button(crash.symbolicated ? 'Read symbolicated' : 'Read crash', () => { this.selectedLog = null; this.read('crashFile', { id: crash.id, raw: !crash.symbolicated }, 'Crash report'); }));
        g.rows.push(row);
      }
    } else errors.push(element('p', 'error', 'Crash reports could not be loaded.'));
    if (traces.status === 'fulfilled') {
      for (const trace of traces.value.traces || []) {
        const g = group(trace.app?.bundleId, trace.app?.name); g.traces += 1;
        const row = element('div', 'service-row');
        row.append(element('span', '', `Trace · ${trace.note || 'debug trace'}${trace.app?.build ? ` · build ${trace.app.build}` : ''}`));
        if (trace.build?.buildId) row.append(button('Build details', () => {
          this.selectedLog = null; this.read('artifactManifest', { id: trace.build.buildId }, `Build ${trace.build.buildId}`);
        }));
        for (const file of trace.files || []) row.append(button(file.path, () => {
          this.selectedLog = null; this.read('traceFile', { id: trace.id, path: file.path }, file.path);
        }));
        g.rows.push(row);
      }
    } else errors.push(element('p', 'error', 'Debug traces could not be loaded.'));
    const rows = [...groups.values()].map(g => {
      const details = element('details', 'service-project');
      details.append(element('summary', '', `${g.name} · ${plural(g.crashes, 'crash')} · ${plural(g.traces, 'trace')}`), ...g.rows);
      return details;
    });
    this.items(this.reports).replaceChildren(...errors, ...(rows.length ? rows : [element('p', 'hint', 'No reports yet.')]));
    const total = [...groups.values()].reduce((sum, g) => sum + g.crashes + g.traces, 0);
    if (this.navButtons) this.navButtons.reports.textContent = total ? `Reports · ${total}` : 'Reports';
  }
  renderDebug(result) {
    const rows = [];
    if (result.status === 'fulfilled') {
      for (const app of result.value.apps || []) {
        const row = element('div', 'service-row');
        row.append(element('span', '', `${app.appName || app.bundleId} · ${app.device || ''}`));
        const inspect = button('Inspect', async () => {
          this.selectedLog = null;
          await this.read('debugCall', { id: app.id, endpoint: '_info', arguments: {} }, app.appName || app.bundleId);
        });
        inspect.disabled = !app.attachable;
        row.append(inspect);
        rows.push(row);
      }
      if (result.value.error) rows.push(element('p', 'hint', String(result.value.error)));
    } else rows.push(element('p', 'error', 'Running apps could not be loaded.'));
    this.items(this.debug).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No debug apps running.')]));
  }
}
