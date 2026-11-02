'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
require('./helpers');
const { ledgerOnlyDevices } = require('../server');

test('ledgerOnlyDevices keeps only device-name strings', () => {
  assert.deepEqual(ledgerOnlyDevices({ ledgerOnlyDevices: ['ATV', 3, null, 'Den TV'] }), ['ATV', 'Den TV']);
});
test('ledgerOnlyDevices is empty when unset or malformed', () => {
  assert.deepEqual(ledgerOnlyDevices({}), []);
  assert.deepEqual(ledgerOnlyDevices({ ledgerOnlyDevices: 'ATV' }), []);
  assert.deepEqual(ledgerOnlyDevices(undefined), []);
});
