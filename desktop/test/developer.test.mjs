import { test } from 'node:test';
import assert from 'node:assert/strict';
import { DeveloperView } from '../ui/developer.js';

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
  view.renderProjects([{ name: 'Sample', branches: ['main'], deployDefaults: { mcp: true }, platforms: [
    { platform: 'iPad', device: 'AVP' }, { platform: 'visionOS', device: 'AVP' },
  ] }]);
  const controls = view.projects.all();
  const selects = controls.filter(n => n.tagName === 'select');
  selects[0].value = 'main'; selects[1].value = '1';
  await controls.find(n => n.tagName === 'button' && n.textContent === 'Build & Install').listeners.click();
  assert.equal(calls.length, 1);
  assert.equal(calls[0].operation, 'build');
  assert.deepEqual(calls[0].arguments, { repo: 'Sample', device: 'AVP', targetPlatform: 'visionOS', branch: 'main',
    run: false, mcp: true, inject: false, buildDebug: false });
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
