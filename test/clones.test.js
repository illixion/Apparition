'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
require('./helpers'); // sets the env first
const { staleClones } = require('../server');

const DAY = 86400000;
const now = 100 * DAY;

test('only clones untouched for longer than the limit are stale', () => {
  const entries = [
    { dir: '/c/App/old', mtimeMs: now - 8 * DAY },
    { dir: '/c/App/fresh', mtimeMs: now - 2 * DAY },
  ];
  assert.deepEqual(staleClones(entries, now, 7 * DAY, new Set()), ['/c/App/old']);
});

test('a clone a build is using is never stale, however old', () => {
  const entries = [{ dir: '/c/App/busy', mtimeMs: now - 30 * DAY }];
  assert.deepEqual(staleClones(entries, now, 7 * DAY, new Set(['/c/App/busy'])), []);
});
