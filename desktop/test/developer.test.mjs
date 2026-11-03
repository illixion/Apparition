import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const {ProjectList, targetKey} = createRequire(import.meta.url)('../../store/public/shared/projects.js');
import { DeveloperView, jobKind } from '../ui/developer.js';

// A DOM event fixture for the controller's credential and deployment boundaries.
// Rendering and platform appearance are deliberately outside these unit tests.
class Node {
  constructor(tag) { this.tagName = tag; this.children = []; this.listeners = {}; this._text = ''; this.value = ''; }
  set textContent(value) { this._text = String(value); this.children = []; }
  get textContent() { return this._text + this.children.map(c => c.textContent).join(''); }
  append(...nodes) {
    this.children.push(...nodes);
    if (this.tagName === 'select' && this.value === '') this.value = nodes[0]?.value || '';
  }
  replaceChildren(...nodes) { this._text = ''; this.children = nodes; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  setAttribute(name, value) { this[name] = value; }
  querySelector(selector) { return this.all().find(n => (n.className || '').split(' ').includes(selector.slice(1))); }
  all() { return this.children.flatMap(n => [n, ...n.all()]); }
}
globalThis.document = { createElement: tag => new Node(tag), createTextNode: text => {
  const node = new Node('#text'); node.textContent = text; return node;
} };

// Controller tests use the real shared selection/submission state machine with a
// small paint adapter. Browser checks cover the shared HTML/CSS separately.
globalThis.ApparitionProjects = { ProjectList: function(root, options) {
  const list = Object.assign(Object.create(ProjectList.prototype), {
    root, options, selections:{}, pending:new Set(), messages:new Map(), query:'', sort:'name',
    render() {
      if (!this.state) return;
      root.replaceChildren(...this.state.repos.map(repo => {
        const card = new Node('article'); options.extend?.(card, repo); return card;
      }));
    },
  });
  return list;
} };

test('the password field clears before connect and only the non-secret address is restored', async () => {
  const root = new Node('main'); let view;
  const invoke = async (command, args) => {
    if (command === 'service_settings') return { url: 'https://build.example' };
    if (command === 'service_connect') {
      assert.equal(view.password.value, '');
      assert.equal(args.password, 'fixture password');
      return { url: args.url, capabilities: ['builds'] };
    }
    if (command === 'service_request') {
      if (args.operation === 'state') return { repos: [], builds: [] };
      return {};
    }
  };
  view = new DeveloperView(root, invoke);
  await Promise.resolve();
  assert.equal(view.address.value, 'https://build.example');
  view.password.value = 'fixture password';
  await view.signIn(); view.show(false);
  assert.equal(view.password.value, ''); assert.equal(view.connected, true);
  assert.ok(!root.textContent.includes('fixture password'));
  await view.signOut();
  assert.equal(view.connected, false); assert.equal(view.content.hidden, true);
});

test('deployment preserves device and platform when one device has multiple targets', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_request') { calls.push(args); return { jobs: [{ id: 7 }] }; }
    return {};
  });
  view.capabilities.add('entry-patching');
  view.renderProjects([{ name: 'Sample', branches: ['main'], deployDefaults: { mcp: true }, platforms: [
    { platform: 'iPad', device: 'AVP' }, { platform: 'visionOS', device: 'AVP' },
  ] }]);
  const repo = view.repos[0], list = view.projectList, selection = list.selection(repo);
  selection.branch = 'main'; selection.injectBackend = 'apparition';
  selection.chosen.add(targetKey(repo.platforms[1]));
  await list.submit(repo, true);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].operation, 'build');
  assert.deepEqual(calls[0].arguments, { repo: 'Sample', device: 'AVP', targetPlatform: 'visionOS', branch: 'main',
    run: false, mcp: true, inject: false, injectBackend: 'apparition', jit: false, buildDebug: false });
  assert.equal(view.selectedLog, 7);
});

test('debug inspection uses the public app identifier and respects unattachable apps', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_request') calls.push(args);
    return {};
  });
  view.connected = true;
  view.renderDebug({ status: 'fulfilled', value: { apps: [
    { id: 'public-key-id', appName: 'Sample', attachable: true },
    { id: null, appName: 'Unavailable', attachable: false },
  ] } });
  const buttons = view.debug.all().filter(n => n.tagName === 'button');
  assert.equal(buttons[1].disabled, true);
  await buttons[0].listeners.click();
  assert.deepEqual(calls[0], { operation: 'debugCall', arguments: { id: 'public-key-id', endpoint: '_info', arguments: {} } });
});

test('artifact downloads pass only the build identity through the native bridge', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_settings' || command === 'host_status') return {};
    calls.push({ command, args }); return { path: '/chosen/app.ipa' };
  });
  const download = view.downloadButton('build-id');
  await download.listeners.click();
  assert.deepEqual(calls, [{ command: 'service_download', args: { id: 'build-id' } }]);
  assert.equal(download.disabled, false);
  assert.equal(download.textContent, 'Download');
  assert.match(view.note.textContent, /Saved verified artifact/);
});

test('registered builds are visible without queue jobs and do not duplicate queued identities', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_settings' || command === 'host_status') return {};
    calls.push({ command, args }); return { path: '/chosen/app.ipa' };
  });
  view.renderArtifacts({ status: 'fulfilled', value: { builds: [
    { buildId: 'in-queue', app: { name: 'Queued' } }, { buildId: 'external', app: { name: 'Xcode build' } },
  ] } }, [{ buildId: 'in-queue' }]);
  assert.ok(view.artifacts.textContent.includes('Xcode build'));
  assert.ok(!view.artifacts.textContent.includes('Queued'));
  await view.artifacts.all().find(node => node.tagName === 'button' && node.textContent === 'Download').listeners.click();
  assert.deepEqual(calls[0], { command: 'service_download', args: { id: 'external' } });
});

test('project registration sends the Mac checkout and explicit targets without building', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_settings' || command === 'host_status') return {};
    calls.push({ command, args }); return { project: { name: 'Example' } };
  });
  const fields = view.registration.all().filter(node => node.tagName === 'input');
  ['Example', '~/Projects/Example', 'main, feature/ui', 'visionOS', 'AVP', 'com.example.app'].forEach((value, index) => fields[index].value = value);
  const form = view.registration.all().find(node => node.tagName === 'form');
  await form.listeners.submit({ preventDefault() {} });
  assert.deepEqual(calls, [{ command: 'service_request', args: { operation: 'registerProject', arguments: {
    name: 'Example', path: '~/Projects/Example', branches: ['main', 'feature/ui'],
    platforms: [{ platform: 'visionOS', device: 'AVP', bundleId: 'com.example.app' }],
  } } }]);
  assert.match(view.note.textContent, /Registered Example/);
  assert.equal(view.registration.open, false);
});

test('signing is offered only with build-and-sign material, and sends just the chosen device', async () => {
  const calls = [];
  let summary = null;
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    calls.push([command, args]);
    if (command === 'build_signing_summary') return summary;
    if (command === 'paired_devices') return [{ udid: 'U1', name: 'AVP', platform: 'visionOS' }, { udid: 'U2', name: 'ATV', platform: 'tvOS' }];
    if (command === 'hand_off_build_signing') return { device: 'ATV', certificates: 1, profiles: 3 };
    return {};
  });
  await view.loadSigning();
  assert.equal(view.signing.hidden, true);
  summary = { dir: '/Users/me/.config/build-signing', certificates: ['development.p12'], profiles: ['a.mobileprovision', 'b.mobileprovision', 'c.mobileprovision'] };
  await view.loadSigning();
  assert.equal(view.signing.hidden, false);
  assert.match(view.signingSummary.textContent, /1 certificate and 3 profiles/);
  assert.equal(view.signingDevice.children.length, 2);
  view.signingDevice.value = 'U2';
  await view.sendSigning();
  assert.deepEqual(calls.at(-1), ['hand_off_build_signing', { udid: 'U2' }]);
  assert.match(view.signingNote.textContent, /Sent 1 certificate\(s\) and 3 profile\(s\) to ATV/);
  assert.equal(view.signingSend.disabled, false);
});

test('re-signing offers only the devices a project installs this app on and sends five plain fields', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command !== 'service_request') return {};
    calls.push(args);
    if (args.operation === 'resignOptions') return { profiles: [
      { id: 'old', name: 'Old', appId: 'com.example.*', expired: true, debuggable: true },
      { id: 'other-dev', name: 'Other', appId: 'com.example.Other', expired: false, debuggable: true } ] };
    if (args.operation === 'resign') return { jobs: [{ id: 11 }] };
    return {};
  });
  view.capabilities = new Set(['resign']);
  view.renderProjects([{ name: 'Sample', branches: [], platforms: [
    { platform: 'visionOS', device: 'AVP', bundleId: 'com.example.app' },
    { platform: 'iOS', device: 'Phone', bundleId: 'com.example.app' } ] }]);
  assert.equal(view.resignButton({ buildId: 'b1' }).textContent, 'Re-sign…');
  view.capabilities = new Set();
  assert.equal(view.resignButton({ buildId: 'b1' }), null, 'no button without the capability');
  view.capabilities = new Set(['resign']);
  await view.openResign({ buildId: 'b1', bundleId: 'com.example.app', platform: 'visionOS', name: 'Sample', label: '#4' });
  assert.deepEqual(view.resignDevice.children.map(o => o.value), ['AVP']);
  assert.deepEqual(view.resignProfile.children.map(o => o.value), ['other-dev'], 'expired profiles are not offered');
  view.resignDevice.value = 'AVP'; view.resignProfile.value = 'other-dev'; view.resignData.value = 'keep';
  await view.submitResign(true);
  const sent = calls.find(c => c.operation === 'resign');
  assert.deepEqual(sent.arguments, { id: 'b1', device: 'AVP', profile: 'other-dev', dataMode: 'keep', dryRun: true });
  assert.equal(view.selectedLog, 11);
  await view.openResign({ buildId: 'b2', bundleId: 'com.example.unknown', platform: 'visionOS' });
  assert.equal(view.resignGo.disabled, true, 'an app no project installs has nothing to sign for');
});

test('Build only keeps the IPA without installing or launching', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_request') { calls.push(args); return { jobs: [{ id: 3 }] }; }
    return {};
  });
  view.renderProjects([{ name: 'Sample', deployDefaults: { launch: true, mcp: true }, platforms: [{ platform: 'iOS', device: 'Phone' }] }]);
  view.projectList.selection(view.repos[0]).chosen.add(targetKey(view.repos[0].platforms[0]));
  await view.projectList.submit(view.repos[0], false);
  assert.deepEqual(calls[0].arguments, { repo: 'Sample', device: 'Phone', targetPlatform: 'iOS', branch: 'current',
    run: false, mcp: true, jit: false, inject: false, injectBackend: 'dynamic', buildDebug: false, deploy: false });
});

const targetButtons = (view, text) => view.projects.all().filter(n => n.className === 'dev-target')
  .flatMap(row => row.all()).filter(n => n.tagName === 'button' && n.textContent === text);
test('a target re-signs its installed kept build, or builds first when the Mac kept none', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command !== 'service_request') return {};
    calls.push(args);
    if (args.operation === 'resignOptions') return { profiles: [{ id: 'other-dev', name: 'Other', appId: 'com.example.Other', expired: false, debuggable: true }] };
    if (args.operation === 'resign') return { jobs: [{ id: 20 }, { id: 21 }] };
    return {};
  });
  const repo = { name: 'Sample', git: { branch: 'main', commit: 'abc1234' }, platforms: [
    { platform: 'visionOS', device: 'AVP', bundleId: 'com.example.app' }, { platform: 'iOS', device: 'Phone', bundleId: 'com.example.app' } ] };
  view.capabilities = new Set(['resign']);
  view.installs = { AVP: { 'com.example.app': { commit: 'abc1234', branch: 'main', buildId: 'kept-1' } } };
  view.artifactList = [{ buildId: 'kept-1', platform: 'visionOS', app: { bundleId: 'com.example.app', build: 'abc1234' } }];
  view.renderProjects([repo]);
  let resigns = targetButtons(view, 'Re-sign…');
  assert.equal(resigns.length, 1, 'without resign-build only the target with a kept build offers it');
  view.capabilities = new Set(['resign', 'resign-build']);
  view.renderProjects([repo]);
  resigns = targetButtons(view, 'Re-sign…');
  assert.equal(resigns.length, 2);
  await resigns[0].listeners.click();
  assert.match(view.resignSource.textContent, /kept-1/);
  view.resignDevice.value = 'AVP'; view.resignProfile.value = 'other-dev'; view.resignData.value = 'keep';
  await view.submitResign(false);
  assert.deepEqual(calls.filter(c => c.operation === 'resign').at(-1).arguments, { id: 'kept-1', device: 'AVP', profile: 'other-dev', dataMode: 'keep', dryRun: false });
  await resigns[1].listeners.click();
  assert.deepEqual(view.resignDevice.children.map(o => o.value), ['Phone']);
  assert.match(view.resignSource.textContent, /builds Sample from its current checkout first/);
  view.resignDevice.value = 'Phone';
  await view.submitResign(false);
  assert.deepEqual(calls.filter(c => c.operation === 'resign').at(-1).arguments, { repo: 'Sample', platform: 'iOS', device: 'Phone', profile: 'other-dev', dataMode: 'keep', dryRun: false });
  assert.equal(view.selectedLog, 21, 'the re-sign, not the build before it, is followed');
  assert.match(view.note.textContent, /Queued build #20, then re-sign #21/);
});

test('jobs say what they do', () => {
  assert.equal(jobKind({ noDeploy: true }), 'Build only');
  assert.equal(jobKind({}), 'Build & install');
  assert.equal(jobKind({ noDeploy: true, forResign: true }), 'Build for re-sign');
  assert.equal(jobKind({ resign: { dryRun: true } }), 'Re-sign check');
});

test('sections switch one pane at a time, and activity lists running builds first', () => {
  const view = new DeveloperView(new Node('main'), async () => ({}));
  assert.equal(view.panes.projects.hidden, false); assert.equal(view.panes.activity.hidden, true);
  view.select('activity');
  assert.equal(view.panes.projects.hidden, true); assert.equal(view.panes.activity.hidden, false);
  assert.equal(view.navButtons.activity['aria-selected'], 'true');
  const old = Array.from({ length: 10 }, (_, i) => ({ id: i + 1, repo: 'R', platform: 'iOS', status: 'success' }));
  view.renderJobs([...old.reverse(), { id: 30, repo: 'R', platform: 'iOS', status: 'running' }]);
  const rows = view.items(view.jobs).children;
  assert.match(rows[0].textContent, /running.*#30/);
  assert.equal(rows.length, 10, 'one running, eight finished, and a button for the rest');
  assert.equal(rows.at(-1).textContent, 'Show 2 older');
});

const jobsOf = view => view.items(view.jobs).children;
test('an unchanged section is not repainted, so what the person opened stays open', () => {
  const view = new DeveloperView(new Node('main'), async () => ({}));
  const traces = { status: 'fulfilled', value: { traces: [{ id: 't1', app: { bundleId: 'a.b', name: 'App' }, files: [] }] } };
  const crashes = { status: 'fulfilled', value: { crashLogs: [{ id: 'c1', bundleId: 'a.b', appName: 'App', exception: 'EXC_BAD_ACCESS' }] } };
  view.renderReports(traces, crashes);
  const group = view.items(view.reports).children[0];
  group.open = true; group.listeners.toggle();
  view.renderReports(traces, crashes);
  assert.equal(view.items(view.reports).children[0], group, 'the same data leaves the DOM alone');
  view.signatures = {};
  view.renderReports(traces, crashes);
  assert.equal(view.items(view.reports).children[0].open, true, 'even a forced repaint restores the open group');
  assert.equal(view.navButtons.reports.textContent, 'Reports · 2');
});

test('a log opens inside the row that asked for it, and Hide closes it', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_request') { calls.push(args.operation); return { status: 'running', chunk: 'line one' }; }
    return {};
  });
  view.connected = true;
  view.jobsList = [{ id: 5, repo: 'R', platform: 'iOS', status: 'running' }, { id: 4, repo: 'R', platform: 'iOS', status: 'success' }];
  const failed = { status: 'rejected', reason: 'offline' };
  view.last = { repos: [], traces: failed, crashes: failed, debug: failed, artifacts: failed };
  view.renderJobs(view.jobsList);
  const logButton = row => row.all().find(n => n.tagName === 'button' && /^(Log|Hide log)$/.test(n.textContent));
  await logButton(jobsOf(view)[1]).listeners.click();
  assert.deepEqual(calls, ['buildLog']);
  assert.ok(jobsOf(view)[1].all().includes(view.viewer), 'the viewer sits under job #4, not at the foot of the page');
  assert.ok(!jobsOf(view)[0].all().includes(view.viewer));
  assert.equal(view.detail.textContent, 'line one');
  assert.equal(view.viewerTitle.textContent, 'Build #4 · running');
  assert.equal(logButton(jobsOf(view)[1]).textContent, 'Hide log');
  view.closeView();
  assert.equal(view.viewer.hidden, true);
});

test('queueing a build follows its log in Activity without asking the service twice', () => {
  const view = new DeveloperView(new Node('main'), async () => ({}));
  view.followJob(9);
  assert.equal(view.selectedLog, 9);
  assert.equal(view.viewKey(), 'job:9|log');
});

test('Apparition Store is introduced once connected, with its GitHub link', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    calls.push([command, args]);
    if (command === 'service_connect') return { url: 'http://localhost:8484/', capabilities: [] };
    return command === 'service_request' ? { repos: [], builds: [] } : {};
  });
  view.password.value = 'pw';
  await view.signIn(); view.show(false);
  assert.match(view.about.textContent, /About Apparition Store/);
  assert.ok(view.panes.setup.children.includes(view.about));
  await view.about.all().find(n => n.tagName === 'button' && n.textContent === 'Apparition Store on GitHub').listeners.click();
  assert.deepEqual(calls.at(-1), ['open_developer_link', { which: 'store' }]);
  assert.match(view.connected_label.textContent, /Apparition Store at http:\/\/localhost:8484\//);
});


test('patching uses only the configured target and does not enqueue a build', async () => {
  const calls = [];
  const view = new DeveloperView(new Node('main'), async (command, args) => {
    if (command === 'service_request') calls.push(args);
    return { content: [{ type: 'text', text: '{"ok":true,"applied":["Sample"]}' }] };
  });
  view.capabilities.add('entry-patching');
  view.renderProjects([{ name: 'Sample', platforms: [{ platform: 'visionOS', device: 'AVP' }] }]);
  await view.projects.all().find(n => n.tagName === 'button' && n.textContent === 'Patch running app').listeners.click();
  assert.deepEqual(calls.map(c => c.operation), ['inject']);
  assert.deepEqual(calls[0].arguments, { repo: 'Sample', device: 'AVP', targetPlatform: 'visionOS' });
});
