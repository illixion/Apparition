import { test } from 'node:test';
import assert from 'node:assert/strict';
import { UpdateBanner, UpdatePreferences } from '../ui/updater.js';

function fixture(busy = () => false, invoke) {
  const text = {};
  const button = { addEventListener() {} };
  const root = { querySelector: id => id === '#update-text' ? text : button };
  const calls = [];
  const banner = new UpdateBanner(root, invoke ?? (async command => calls.push(command)), busy);
  return { banner, root, text, button, calls };
}
const available = { version: '1.0.0', phase: 'available', canInstall: true, message: 'Verified release available.' };

test('update notice persists during download and failures, and retry installs rather than opening a URL', async () => {
  const { banner, root, button, text, calls } = fixture();
  banner.draw(available);
  assert.equal(root.hidden, false);
  assert.equal(button.textContent, 'Update & restart');
  await banner.activate();
  assert.deepEqual(calls, ['install_update']);
  banner.draw({ ...available, phase: 'downloading' });
  assert.equal(button.disabled, true);
  assert.equal(root.hidden, false);
  await banner.activate();
  assert.equal(calls.length, 1);
  banner.draw({ ...available, phase: 'error', message: 'Signature verification failed' });
  assert.match(text.textContent, /Signature verification failed/);
  assert.equal(button.disabled, false);
  assert.equal(root.hidden, false);
  await banner.activate();
  assert.deepEqual(calls, ['install_update', 'install_update']);
});

test('busy installs and signing prompts prevent restarting; unsupported packages only open downloads', async () => {
  const f = fixture(() => true);
  f.banner.draw(available);
  await f.banner.activate();
  assert.deepEqual(f.calls, []);
  assert.match(f.text.textContent, /Finish the current operation/);
  f.banner.draw({ ...available, canInstall: false });
  await f.banner.activate();
  assert.deepEqual(f.calls, ['open_downloads']);
});

test('manual check errors can be retried without an update and command failures remain visible', async () => {
  const f = fixture();
  f.banner.draw({ phase: 'error', message: 'Offline' });
  await f.banner.activate();
  assert.deepEqual(f.calls, ['check_update']);
  const failed = fixture(() => false, async () => { throw { message: 'Installation folder is read-only' }; });
  failed.banner.draw(available);
  await failed.banner.activate();
  assert.equal(failed.root.hidden, false);
  assert.equal(failed.button.disabled, false);
  assert.match(failed.text.textContent, /read-only/);
});

function preferences(initial = false, fail = () => false) {
  const inputs = [{ checked: false }, { checked: false }];
  const notices = [{}, {}];
  const listeners = {};
  const root = {
    addEventListener(name, callback) { listeners[name] = callback; },
    querySelectorAll: selector => selector.includes('auto-updates') ? inputs : notices,
  };
  const status = {};
  const buttons = [{ addEventListener() {} }, { addEventListener() {} }];
  const dialog = {
    open: false, listeners: {},
    querySelector: selector => selector.includes('check-status') ? status : buttons[0],
    querySelectorAll: () => buttons,
    addEventListener(name, callback) { this.listeners[name] = callback; },
    showModal() { this.open = true; },
    close() { this.open = false; this.listeners.close?.(); },
  };
  const calls = [];
  let saved = initial;
  const invoke = async (command, args) => {
    calls.push([command, args]);
    if (fail(command)) throw { message: 'Fixture failure' };
    if (command === 'set_auto_updates') saved = args.on;
    return saved;
  };
  const view = new UpdatePreferences(root, dialog, invoke);
  return { view, dialog, inputs, notices, calls, status, buttons };
}

test('opening preferences and declining the Get started prompt makes no update request', async () => {
  const f = preferences();
  await f.view.ready;
  const starting = f.view.beforeStart();
  await Promise.resolve();
  assert.equal(f.dialog.open, true);
  f.dialog.close();
  await starting;
  assert.deepEqual(f.calls, [['auto_updates', undefined]]);
  assert.ok(f.inputs.every(input => !input.checked));
});

test('a one-time check does not opt in and still prompts on the next Get started', async () => {
  const f = preferences();
  await f.view.ready;
  let starting = f.view.beforeStart();
  await Promise.resolve();
  await f.view.checkOnce();
  await starting;
  assert.deepEqual(f.calls.map(call => call[0]), ['auto_updates', 'check_update']);
  assert.equal(f.view.on, false);
  starting = f.view.beforeStart();
  await Promise.resolve();
  assert.equal(f.dialog.open, true);
  f.dialog.close();
  await starting;
});

test('the Done and Companion controls share persisted consent and opt-in bypasses the prompt', async () => {
  const f = preferences();
  await f.view.ready;
  await f.view.save(true);
  assert.ok(f.inputs.every(input => input.checked && !input.disabled));
  assert.deepEqual(f.calls.at(-1), ['set_auto_updates', { on: true }]);
  await f.view.beforeStart();
  assert.equal(f.dialog.open, false);
  await f.view.save(false);
  assert.ok(f.inputs.every(input => !input.checked));
  assert.deepEqual(f.calls.at(-1), ['set_auto_updates', { on: false }]);
});

test('failed consent writes restore the controls and a failed one-time check allows skipping', async () => {
  const f = preferences(false, command => command !== 'auto_updates');
  await f.view.ready;
  await f.view.save(true);
  assert.ok(f.inputs.every(input => !input.checked && !input.disabled));
  assert.match(f.notices[0].textContent, /Fixture failure/);
  const starting = f.view.beforeStart();
  await Promise.resolve();
  await f.view.checkOnce();
  assert.equal(f.dialog.open, true);
  assert.match(f.status.textContent, /Fixture failure/);
  assert.ok(f.buttons.every(button => !button.disabled));
  f.dialog.close();
  await starting;
});
