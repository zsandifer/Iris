// status.js: plain-language activity for Iris's browser tools.
import test from 'node:test';
import assert from 'node:assert/strict';
import { describe } from '../../web/js/status.js';

const say = (name, summary = '') => describe({ kind: 'mcp', name, summary });

test('browser tools read as what they do (Playwright names and earlier ones)', () => {
  assert.deepEqual(say('browser/browser_navigate', 'https://www.example.com/a'), ['Opening example.com', 'Opened example.com']);
  assert.deepEqual(say('browser/navigate', 'https://example.com'), ['Opening example.com', 'Opened example.com']);
  assert.deepEqual(say('browser/fill_saved_login'), ['Signing you in', 'Signed you in']);
  assert.deepEqual(say('iris_browser/fill_saved_card'), ['Filling in your card', 'Filled in your card']);
  assert.deepEqual(say('browser/fill_card'), ['Filling in your card', 'Filled in your card']);
  assert.deepEqual(say('browser/browser_take_screenshot'), ['Looking at the page', 'Looked at the page']);
  assert.deepEqual(say('browser/browser_fill_form'), ['Filling in a form', 'Filled in a form']);
  assert.deepEqual(say('browser/browser_hover'), ['Browsing the web', 'Browsed the web']);
});
