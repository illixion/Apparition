'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
require('./helpers'); // sets the env first
const { deployDefaults } = require('../server');

test('deploy checkboxes start from the built-in defaults: MCP on, the rest off', () => {
  assert.deepEqual(deployDefaults({ name: 'A' }),
    { launch: false, mcp: true, buildDebug: false, inject: false });
});

test('a repo\'s deployDefaults overrides individual keys, and ignores non-booleans', () => {
  assert.deepEqual(deployDefaults({ name: 'A', deployDefaults: { mcp: false, inject: true, launch: 'yes' } }),
    { launch: false, mcp: false, buildDebug: false, inject: true });
});
