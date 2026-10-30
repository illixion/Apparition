import { test } from 'node:test';
import assert from 'node:assert/strict';
import { guardRepeatedActivation } from '../ui/clicks.js';

function window() {
  const listeners = {};
  const root = { addEventListener: (type, listener, capture) => { assert.equal(capture, true); listeners[type] = listener; } };
  guardRepeatedActivation(root);
  const fire = (type, fields) => {
    const event = { ...fields, dropped: false, preventDefault() { this.dropped = true; }, stopImmediatePropagation() {} };
    listeners[type](event);
    return event.dropped;
  };
  return fire;
}
const button = { closest: selector => selector.includes('button') ? button : null, tagName: 'BUTTON' };
const text = { closest: selector => selector === 'form' ? {} : null, tagName: 'INPUT' };
const area = { closest: selector => selector === 'form' ? {} : null, tagName: 'TEXTAREA' };
const paragraph = { closest: () => null, tagName: 'P' };

test('the second click of a double-click never reaches a control', () => {
  const fire = window();
  assert.equal(fire('click', { detail: 1, target: button }), false);
  assert.equal(fire('click', { detail: 2, target: button }), true);
  assert.equal(fire('click', { detail: 3, target: button }), true);
  // Keyboard activation reports no click count.
  assert.equal(fire('click', { detail: 0, target: button }), false);
  // Double-clicking text still selects words.
  assert.equal(fire('click', { detail: 2, target: paragraph }), false);
});

test('a held key presses a control or submits a form once', () => {
  const fire = window();
  assert.equal(fire('keydown', { key: 'Enter', repeat: false, target: button }), false);
  assert.equal(fire('keydown', { key: 'Enter', repeat: true, target: button }), true);
  assert.equal(fire('keydown', { key: ' ', repeat: true, target: button }), true);
  assert.equal(fire('keydown', { key: 'Enter', repeat: true, target: text }), true);
  assert.equal(fire('keydown', { key: ' ', repeat: true, target: text }), false);
  assert.equal(fire('keydown', { key: 'Enter', repeat: true, target: area }), false);
  assert.equal(fire('keydown', { key: 'a', repeat: true, target: button }), false);
});
