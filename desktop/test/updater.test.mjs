import { test } from 'node:test';
import assert from 'node:assert/strict';
import { UpdateBanner } from '../ui/updater.js';

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
