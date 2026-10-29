// The build service is reached only through Rust. No cookies, bearer tokens or
// remote HTML enter this view; all service-provided strings become text nodes.
const element = (tag, className = '', text = '') => {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
};
const button = (text, action) => {
  const node = element('button', 'button quiet', text);
  node.type = 'button';
  node.addEventListener('click', action);
  return node;
};

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
    root.append(element('h1', '', 'Developer'));
    root.append(element('p', 'lede', 'Build on your Mac, install on your devices, and bring diagnostics back to the same project.'));
    const form = element('form', 'group service-connect');
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
    this.disconnect = button('Disconnect', () => this.signOut());
    this.disconnect.hidden = true;
    form.append(addressLabel, passwordLabel, this.connect, this.disconnect);
    form.append(element('p', 'hint', 'Use your appstore service password. This connection is separate from the companion relay. Sign in again after quitting Apparition.'));
    form.addEventListener('submit', event => { event.preventDefault(); this.signIn(); });
    this.note = element('p', 'status'); this.note.setAttribute('role', 'status');
    this.content = element('div', 'service-content'); this.content.hidden = true;
    this.projects = this.section('Projects');
    this.devices = this.section('Devices');
    this.jobs = this.section('Builds');
    this.artifacts = this.section('Registered builds'); this.artifacts.hidden = true;
    this.reports = this.section('Reports');
    this.debug = this.section('Running apps');
    this.detail = element('pre', 'service-detail selectable');
    this.detail.tabIndex = 0; this.detail.hidden = true;
    this.content.append(this.projects, this.devices, this.jobs, this.artifacts, this.reports, this.debug, this.detail);
    root.append(form, this.note, this.content);
    invoke('service_settings').then(settings => { if (settings.url) this.address.value = settings.url; }).catch(() => {});
  }

  section(title) {
    const node = element('section', 'group');
    node.append(element('h2', 'small', title), element('div', 'service-items'));
    return node;
  }
  items(section) { return section.querySelector('.service-items'); }
  status(text, error = false) { this.note.textContent = text; this.note.className = `status ${error ? 'error' : ''}`; }
  request(operation, args = {}) { return this.invoke('service_request', { operation, arguments: args }); }
  show(visible) {
    this.visible = visible;
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
      this.artifacts.hidden = !this.capabilities.has('artifact-manifests');
      this.epoch += 1;
      this.address.disabled = this.password.disabled = true;
      this.connect.hidden = true; this.disconnect.hidden = false; this.content.hidden = false;
      this.projectSignature = '';
      this.status(`Connected to ${service.url}`);
      this.show(true);
    } catch (error) { this.status(String(error?.message || error), true); }
    finally { this.connect.disabled = false; }
  }
  async signOut() {
    clearInterval(this.timer);
    this.connected = false;
    this.epoch += 1;
    this.content.hidden = true;
    this.address.disabled = this.password.disabled = false;
    this.connect.hidden = false; this.disconnect.hidden = true;
    this.detail.replaceChildren(); this.detail.hidden = true;
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
      this.renderProjects(state.repos || []);
      this.items(this.devices).replaceChildren(...(state.devices?.list || []).map(device =>
        element('p', 'hint', `${device.name} · ${device.model || ''} · ${device.state || 'unknown'}`)));
      const jobs = (state.builds || []).map(job => {
        const row = element('div', 'service-row');
        row.append(element('span', '', `#${job.id} ${job.repo} · ${job.platform} · ${job.device || ''} · ${job.status}`));
        row.append(button('Log', async () => { this.selectedLog = job.id; await this.read('buildLog', { id: job.id }, `Build #${job.id}`); }));
        if (job.buildId && job.artifact) {
          row.append(button('Build details', () => { this.selectedLog = null; this.read('artifactManifest', { id: job.buildId }, `Build ${job.buildId}`); }));
          row.append(this.downloadButton(job.buildId));
        }
        if (['queued', 'running'].includes(job.status)) row.append(button('Cancel', () => this.action('cancel', { id: job.id })));
        return row;
      });
      this.items(this.jobs).replaceChildren(...(jobs.length ? jobs : [element('p', 'hint', 'No builds yet.')]));
      const results = await Promise.allSettled([this.request('traces'), this.request('crashlogs'), this.request('debugApps'),
        this.capabilities.has('artifact-manifests') ? this.request('artifacts') : Promise.resolve({ builds: [] })]);
      if (!this.connected || !this.visible || epoch !== this.epoch) return;
      this.renderReports(results[0], results[1]);
      this.renderDebug(results[2]);
      this.renderArtifacts(results[3], state.builds || []);
      if (this.selectedLog) {
        const logId = this.selectedLog;
        const log = await this.request('buildLog', { id: logId });
        if (this.selectedLog === logId && this.connected && epoch === this.epoch) this.showDetail(`Build #${logId} · ${log.status}`, log.chunk);
      }
    } catch (error) { this.status(String(error?.message || error), true); }
    finally { this.polling = false; }
  }
  renderProjects(repos) {
    const signature = JSON.stringify(repos.map(r => [r.name, r.branches, r.platforms, r.deployDefaults]));
    if (signature === this.projectSignature) return;
    this.projectSignature = signature;
    const rows = repos.map(repo => {
      const card = element('div', 'service-project');
      card.append(element('h3', '', repo.name));
      const branch = element('select'); branch.setAttribute('aria-label', `${repo.name} branch`);
      for (const name of ['current', ...(repo.branches || []).filter(b => b !== 'current')]) {
        const option = element('option', '', name === 'current' ? 'Current checkout' : name); option.value = name; branch.append(option);
      }
      const target = element('select'); target.setAttribute('aria-label', `${repo.name} target`);
      for (const [index, pc] of (repo.platforms || []).entries()) {
        const option = element('option', '', `${pc.platform} · ${pc.device}`); option.value = String(index); target.append(option);
      }
      const controls = element('div', 'service-row'); controls.append(branch, target);
      const flags = {};
      for (const [key, title] of [['run', 'Launch'], ['mcp', 'Debug access'], ['inject', 'Live patching'], ['buildDebug', 'Debug build']]) {
        const label = element('label', 'check'); const input = element('input'); input.type = 'checkbox';
        input.checked = !!repo.deployDefaults?.[key === 'run' ? 'launch' : key]; flags[key] = input;
        label.append(input, document.createTextNode(title)); controls.append(label);
      }
      const deploy = button('Build & Install', async () => {
        deploy.disabled = true;
        try {
          const selected = repo.platforms[Number(target.value)];
          const reply = await this.action('build', { repo: repo.name, device: selected.device, targetPlatform: selected.platform, branch: branch.value,
            ...Object.fromEntries(Object.entries(flags).map(([key, input]) => [key, input.checked])) });
          if (reply?.jobs?.[0]) { this.selectedLog = reply.jobs[0].id; this.status(`Queued build #${this.selectedLog}.`); }
        } finally { deploy.disabled = !target.value; }
      });
      deploy.disabled = !target.value;
      card.append(controls, deploy);
      return card;
    });
    this.items(this.projects).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'Add a project to the Mac service configuration to start building.')]));
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
  renderArtifacts(result, jobs) {
    const rows = [];
    if (result.status === 'fulfilled') {
      const inJobs = new Set(jobs.map(job => job.buildId).filter(Boolean));
      for (const build of result.value.builds || []) {
        if (inJobs.has(build.buildId)) continue;
        const row = element('div', 'service-row');
        row.append(element('span', '', `${build.app?.name || build.app?.bundleId || 'App'} · ${build.platform || ''} · ${build.app?.build || build.buildId}`));
        row.append(button('Build details', () => { this.selectedLog = null; this.read('artifactManifest', { id: build.buildId }, `Build ${build.buildId}`); }));
        row.append(this.downloadButton(build.buildId));
        rows.push(row);
      }
    } else rows.push(element('p', 'error', 'Registered builds could not be loaded.'));
    this.items(this.artifacts).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No additional registered builds.')]));
  }
  renderReports(traces, crashes) {
    const rows = [];
    if (traces.status === 'fulfilled') {
      for (const trace of traces.value.traces || []) {
        const row = element('details', 'service-project');
        row.append(element('summary', '', `${trace.app?.name || trace.app?.bundleId || 'App'} · ${trace.app?.build || ''} · ${trace.note || 'Debug trace'}`));
        if (trace.build?.buildId) row.append(button('Build details', () => {
          this.selectedLog = null; this.read('artifactManifest', { id: trace.build.buildId }, `Build ${trace.build.buildId}`);
        }));
        for (const file of trace.files || []) row.append(button(file.path, () => {
          this.selectedLog = null; this.read('traceFile', { id: trace.id, path: file.path }, file.path);
        }));
        rows.push(row);
      }
    } else rows.push(element('p', 'error', 'Debug traces could not be loaded.'));
    if (crashes.status === 'fulfilled') {
      for (const crash of crashes.value.crashLogs || []) {
        const row = element('div', 'service-row');
        row.append(element('span', '', `${crash.bundleId || crash.app || 'App'} · ${crash.device || ''} · Crash`));
        row.append(button('Read crash', () => { this.selectedLog = null; this.read('crashFile', { id: crash.id, raw: !crash.symbolicated }, 'Crash report'); }));
        rows.push(row);
      }
    } else rows.push(element('p', 'error', 'Crash reports could not be loaded.'));
    this.items(this.reports).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No reports yet.')]));
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
    this.items(this.debug).replaceChildren(...(rows.length ? rows : [element('p', 'hint', 'No debug apps discovered.')]));
  }
}
