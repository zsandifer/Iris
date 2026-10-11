import test from 'node:test';
import assert from 'node:assert/strict';
import { keyboardOpen } from '../../web/js/viewport.js';

test('phone keyboards substantially shrink the viewport in either orientation', () => {
  assert.equal(keyboardOpen(844, 500), true);
  assert.equal(keyboardOpen(390, 180), true);
  assert.equal(keyboardOpen(1024, 650), true);
});

test('browser chrome and small resizes are not keyboards', () => {
  assert.equal(keyboardOpen(844, 844), false);
  assert.equal(keyboardOpen(844, 750), false);
  assert.equal(keyboardOpen(844, 844 - 844 * .2), false);
  assert.equal(keyboardOpen(390, 270), false);
  assert.equal(keyboardOpen(844, 900), false);
});

test('pinch zoom and unavailable viewport sizes are not keyboards', () => {
  assert.equal(keyboardOpen(844, 422, 2), false);
  assert.equal(keyboardOpen(844, 500, 1.25), false);
  assert.equal(keyboardOpen(844, 500, 1.001), true);
  assert.equal(keyboardOpen(844, 0), false);
});
