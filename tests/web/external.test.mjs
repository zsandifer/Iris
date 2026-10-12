import test from 'node:test';
import assert from 'node:assert/strict';
import { externalUrl, openExternal } from '../../web/js/external.js';

function globals(t, values) {
  const saved = Object.fromEntries(Object.keys(values).map((key) => [key, Object.getOwnPropertyDescriptor(globalThis, key)]));
  for (const [key, value] of Object.entries(values)) Object.defineProperty(globalThis, key, { configurable: true, writable: true, value });
  t.after(() => { for (const [key, desc] of Object.entries(saved)) { if (desc) Object.defineProperty(globalThis, key, desc); else delete globalThis[key]; } });
}

const url = 'https://accounts.example/signin?state=123&next=%2Fhome#approve';
test('iPhone/iPad standalone sign-ins use Safari and preserve the entire link', () => {
  for (const env of [
    { userAgent: 'iPhone', standalone: true },
    { userAgent: 'iPad', standalone: true },
    { userAgent: 'Macintosh', platform: 'MacIntel', maxTouchPoints: 5, standalone: true },
  ]) {
    assert.equal(externalUrl(url, env), url.replace('https:', 'x-safari-https:'));
    assert.equal(externalUrl('http://localhost:7420/login', env), 'x-safari-http://localhost:7420/login');
  }
});
test('regular iOS tabs, desktop, and Android retain ordinary http(s) links', () => {
  for (const env of [
    { userAgent: 'iPhone', standalone: false },
    { userAgent: 'Android', standalone: true },
    { userAgent: 'Macintosh', platform: 'MacIntel', maxTouchPoints: 0, standalone: true },
    { userAgent: 'Windows', standalone: false },
  ]) assert.equal(externalUrl(url, env), url);
});
test('unsafe and malformed links are rejected', () => {
  for (const bad of ['javascript:alert(1)', 'data:text/html,x', 'file:///tmp/key', '/relative', 'https://user:secret@example.com', 'junk']) assert.equal(externalUrl(bad), null);
});
test('opening uses Safari navigation in a PWA and a protected new tab elsewhere', (t) => {
  const opened = [], assigned = [];
  globals(t, { open: (...args) => opened.push(args), location: { assign: (href) => assigned.push(href) }, navigator: { userAgent: 'iPhone', standalone: true } });
  openExternal(url);
  assert.deepEqual(assigned, [url.replace('https:', 'x-safari-https:')]);
  assert.equal(opened.length, 0);
  globalThis.navigator = { userAgent: 'Android', standalone: true };
  openExternal(url);
  assert.deepEqual(opened, [[url, '_blank', 'noopener,noreferrer']]);
  openExternal('javascript:alert(1)');
  assert.equal(opened.length, 1);
});
test('display-mode detection also recognizes an iOS PWA', (t) => {
  globals(t, { navigator: { userAgent: 'iPhone' }, matchMedia: () => ({ matches: true }) });
  assert.equal(externalUrl(url), url.replace('https:', 'x-safari-https:'));
});
