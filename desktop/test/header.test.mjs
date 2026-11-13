import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

// Tauri treats a button that carries `data-tauri-drag-region` as part of the title bar: on
// mouse-down it starts dragging the window and cancels the click. macOS lets the click through
// anyway, Windows does not, so the header's tabs went dead there. Only the header's bare
// surfaces may drag the window; anything pressable inside it must not.
test('no pressable element is a window drag region', () => {
  const html = readFileSync(new URL('../ui/index.html', import.meta.url), 'utf8');
  const tags = html.match(/<[a-z][^>]*>/gi) ?? [];
  const pressable = /^<(button|a|input|select|summary|label)\b|role="(button|tab|switch|checkbox|radio|menuitem)"/i;
  const offenders = tags.filter(tag => pressable.test(tag) && tag.includes('data-tauri-drag-region'));
  assert.deepEqual(offenders, []);
});
