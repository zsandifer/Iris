// web/js/api.js: path safety, stale responses across sign-ins, deadlines, image cache.
import { onFetch, reply, calls, tick } from './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { api, setToken, onUnauthorized, holdBlob, seg, isId } from '../../web/js/api.js';

const revoked = new Set();
const realRevoke = URL.revokeObjectURL;
URL.revokeObjectURL = (u) => { revoked.add(u); realRevoke(u); };

test('isId accepts only server ids', () => {
  assert.ok(isId('0123456789abcdef0123456789abcdef'));
  for (const bad of ['..', '../engines/codex/install?', 'A'.repeat(32), 'a'.repeat(31), '', undefined, null, 42]) {
    assert.equal(isId(bad), false, String(bad));
  }
});

test('seg encodes a segment and refuses dot segments', () => {
  assert.equal(seg('../engines/codex/install?'), '..%2Fengines%2Fcodex%2Finstall%3F');
  assert.equal(new URL(`/api/conversations/${seg('../x')}/read`, 'http://h').pathname, '/api/conversations/..%2Fx/read');
  for (const bad of ['', '.', '..', undefined, null]) assert.throws(() => seg(bad), String(bad));
});

test('a JSON body that finishes after the token changed is dropped', async () => {
  const held = [];
  onFetch((url, opts) => { const r = reply(opts, { body: { secret: 'owner' }, hold: true }); held.push(r); return r; });
  setToken('owner');
  const p = api('GET', '/library');
  await tick();
  setToken('');
  setToken('guest');
  held[0].release();
  await assert.rejects(p, (e) => e.code === 'stale' && e.message === '');
});

test('a text download that finishes after re-pairing is dropped', async () => {
  const held = [];
  onFetch((url, opts) => { const r = reply(opts, { type: 'text/markdown', body: 'owner doc', hold: true }); held.push(r); return r; });
  setToken('owner');
  const p = api('GET', '/library/x/content', undefined, { as: 'text' });
  await tick();
  setToken('guest');
  held[0].release();
  await assert.rejects(p, (e) => e.code === 'stale');
});

test('a late 401 from an earlier token does not sign out the new session', async () => {
  let signedOut = 0;
  onUnauthorized(() => signedOut++);
  let resolve;
  onFetch((url, opts) => new Promise((r) => { resolve = () => r(reply(opts, { status: 401, body: { error: { code: 'unauthorized' } } })); }));
  setToken('owner');
  const p = api('GET', '/feed');
  await tick();
  setToken('guest');
  resolve();
  await assert.rejects(p, (e) => e.code === 'stale');
  assert.equal(signedOut, 0);
});

test('the deadline covers the body, not just the headers', { timeout: 5000 }, async () => {
  onFetch((url, opts) => reply(opts, { body: {}, hold: true }));
  setToken('t');
  const started = Date.now();
  await assert.rejects(api('GET', '/feed', undefined, { timeout: 80 }), (e) => e.code === 'offline');
  await assert.rejects(api('GET', '/x', undefined, { as: 'blob', timeout: 80 }), (e) => e.code === 'offline');
  assert.ok(Date.now() - started < 2000);
});

test('a normal response is returned', async () => {
  onFetch((url, opts) => reply(opts, { body: { ok: 1 } }));
  assert.deepEqual(await api('GET', '/x'), { ok: 1 });
  onFetch((url, opts) => reply(opts, { type: 'text/plain', body: 'hi' }));
  assert.equal(await api('GET', '/x', undefined, { as: 'text' }), 'hi');
});

test('changing the token clears cached images; the next user fetches with its own token', async () => {
  onFetch((url, opts) => reply(opts, { type: 'image/png', body: 'png' }));
  setToken('owner');
  const h = holdBlob('/api/attachments/a');
  const ownerUrl = await h.url;
  h.release();
  const again = holdBlob('/api/attachments/a');
  assert.equal(await again.url, ownerUrl, 'cached for the same user');
  again.release();
  calls.length = 0;
  setToken('guest');
  await tick();
  assert.ok(revoked.has(ownerUrl), 'owner URL revoked');
  const g = holdBlob('/api/attachments/a');
  assert.notEqual(await g.url, ownerUrl);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].auth, 'Bearer guest');
  g.release();
});

test('an image still downloading when the token changes never enters the cache', async () => {
  const held = [];
  onFetch((url, opts) => { const r = reply(opts, { type: 'image/png', body: 'png', hold: true }); held.push(r); return r; });
  setToken('owner');
  const h = holdBlob('/api/attachments/late');
  await tick();
  setToken('guest');
  held[0].release();
  await assert.rejects(h.url, (e) => e.code === 'stale');
  h.release();
  const again = holdBlob('/api/attachments/late');
  assert.equal(held.length, 2, 'the new user requests it again');
  held[1].release();
  await again.url;
  again.release();
});

test('the image cache keeps held URLs and evicts unused ones beyond its cap', async () => {
  onFetch((url, opts) => reply(opts, { type: 'image/png', body: url }));
  setToken('cache');
  await tick(); // the previous user's images are revoked asynchronously
  revoked.clear();
  const keep = holdBlob('/api/attachments/keep');
  const keepUrl = await keep.url;
  const hs = [];
  for (let i = 0; i < 60; i++) { const h = holdBlob(`/api/attachments/${i}`); await h.url; hs.push(h); }
  assert.equal(revoked.size, 0, 'nothing revoked while held');
  for (const h of hs) h.release();
  await tick();
  assert.equal(revoked.size, 21, '61 entries, cap 40: the 21 oldest unused go');
  assert.ok(!revoked.has(keepUrl), 'a held URL is never revoked');
  keep.release();
});
