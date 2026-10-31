'use strict';
// The login page's own script, run against a stub DOM: where does it send the
// browser after a good sign-in? Running the shipped code keeps the check from
// drifting away from what the page really does.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '..', 'public', 'login.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];

async function landing(search) {
  let submit, replacedWith = null;
  const el = { value: 'pw', textContent: '', disabled: false, select() {}, addEventListener: (type, fn) => { if (type === 'submit') submit = fn; } };
  vm.runInNewContext(script, {
    location: { search, replace: url => { replacedWith = url; } },
    document: { getElementById: () => el },
    fetch: async () => ({ ok: true, status: 200, json: async () => ({}) }),
    URLSearchParams, JSON,
  });
  await submit({ preventDefault() {} });
  return replacedWith;
}

test('a signed-in browser goes to the page it came from', async () => {
  assert.equal(await landing(''), '/');
  assert.equal(await landing('?next=%2Fota'), '/ota');
  assert.equal(await landing('?next=%2F%23debug%2Fk1'), '/#debug/k1');
});

test('the next page is never somewhere else', async () => {
  for (const next of ['//evil.example', 'https://evil.example', '/\\evil.example', '\\\\evil.example', 'evil.example',
    '/ok\r\nSet-Cookie: x=1', '/ok\nX: y', '/ok\u0000', 'javascript:alert(1)']) {
    assert.equal(await landing(`?next=${encodeURIComponent(next)}`), '/', JSON.stringify(next));
  }
});
