'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
require('./helpers'); // sets the env first
const { afterInstallDefault } = require('../server');

test('the deploy menu starts from the repo\'s afterInstall, else install', () => {
  assert.equal(afterInstallDefault({ name: 'A' }), 'install');
  assert.equal(afterInstallDefault({ name: 'A', afterInstall: 'debug' }), 'debug');
  assert.equal(afterInstallDefault({ name: 'A', afterInstall: 'run' }), 'run');
  assert.equal(afterInstallDefault({ name: 'A', afterInstall: 'bogus' }), 'install');
});
