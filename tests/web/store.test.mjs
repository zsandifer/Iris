// store.js: message snapshots vs live events, paging, Library reloads,
// sign-in state.
import { onFetch, reply, tick } from './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { setToken } from '../../web/js/api.js';
import {
  S, set, loadMessages, loadOlder, handleEvent, loadLibrary, loadMoreSaved, loadMorePhotos,
  loginMark, applyLoginResponse,
} from '../../web/js/store.js';

setToken('t');
const CID = 'a'.repeat(32);
const msg = (id, at, status = 'complete', upd = at) => ({ id, conversation_id: CID, role: 'assistant', content: '', status, created_at: at, updated_at: upd });
const route = (routes) => onFetch((url, opts) => {
  const u = new URL(url, 'http://iris.test');
  const h = routes[u.pathname];
  if (!h) throw new TypeError('offline');
  return h(u.searchParams, opts);
});

test('a REST snapshot never regresses a reply that events already completed', async () => {
  let release;
  route({ [`/api/conversations/${CID}/messages`]: (q, opts) => new Promise((r) => {
    release = () => r(reply(opts, { body: { messages: [msg('u1', 1), msg('m1', 2, 'streaming')], has_more: false } }));
  }) });
  const p = loadMessages(CID);
  await tick();
  handleEvent({ type: 'message.created', conversation_id: CID, message: msg('m1', 2, 'streaming') });
  handleEvent({ type: 'message.delta', message_id: 'm1', text: 'hi' });
  handleEvent({ type: 'message.completed', conversation_id: CID, message: msg('m1', 2, 'complete', 5) });
  handleEvent({ type: 'message.created', conversation_id: CID, message: msg('u2', 6) });
  release();
  await p;
  const box = S.msgs[CID];
  assert.equal(box.list.find((m) => m.id === 'm1').status, 'complete');
  assert.equal(S.live.m1, undefined, 'no live entry recreated');
  assert.deepEqual(box.list.map((m) => m.id), ['u1', 'm1', 'u2'], 'newer event-only message kept');
  assert.equal(box.loading, false);
});

test('a snapshot finishes a reply whose completion event was missed', async () => {
  handleEvent({ type: 'message.created', conversation_id: CID, message: msg('m9', 7, 'streaming') });
  assert.ok(S.live.m9);
  route({ [`/api/conversations/${CID}/messages`]: (q, opts) => reply(opts, { body: { messages: [msg('u1', 1), msg('m1', 2, 'complete', 5), msg('u2', 6), msg('m9', 7, 'complete', 9)], has_more: false } }) });
  await loadMessages(CID);
  assert.equal(S.msgs[CID].list.find((m) => m.id === 'm9').status, 'complete');
  assert.equal(S.live.m9, undefined);
});

test('loadOlder releases loading when the request fails, and paging works after', async () => {
  set({ msgs: { ...S.msgs, [CID]: { ...S.msgs[CID], hasMore: true } } });
  route({});
  await assert.rejects(loadOlder(CID));
  assert.equal(S.msgs[CID].loading, false);
  route({ [`/api/conversations/${CID}/messages`]: (q, opts) => reply(opts, { body: { messages: [msg('o1', 0)], has_more: false } }) });
  await loadOlder(CID);
  assert.equal(S.msgs[CID].list[0].id, 'o1');
  assert.equal(S.msgs[CID].hasMore, false);
  assert.equal(S.msgs[CID].loading, false);
});

// ---------------------------------------------------------------- Library

const server = { posts: [], photos: [] };
const posts = (hi, lo) => { const a = []; for (let i = hi; i >= lo; i--) a.push({ id: 'p' + i, created_at: i, liked: true }); return a; };
const pid = (i) => String(i).padStart(4, '0');
const photos = (hi, lo) => {
  const a = [];
  for (let i = hi; i >= lo; i--) a.push({ id: pid(i), used_at: Math.floor(i / 2) });
  return a.sort((x, y) => y.used_at - x.used_at || (x.id < y.id ? 1 : -1));
};
let gate = null; // when set, page requests wait for it
const page = (list, q) => {
  const lim = +q.get('limit');
  let l = list;
  if (q.get('before_id')) {
    const b = +q.get('before'); const id = q.get('before_id');
    l = l.filter((p) => p.used_at < b || (p.used_at === b && p.id < id));
  } else if (q.get('before')) l = l.filter((p) => p.created_at < +q.get('before'));
  return [l.slice(0, lim), l.length > lim];
};
const libraryRoutes = {
  '/api/library': (q, opts) => reply(opts, { body: { items: [] } }),
  '/api/feed': async (q, opts) => {
    if (gate && q.get('before')) await gate;
    const [p, more] = page(server.posts.filter((x) => x.liked), q);
    return reply(opts, { body: { posts: p, has_more: more } });
  },
  '/api/attachments': (q, opts) => {
    const [p, more] = page(server.photos, q);
    return reply(opts, { body: { attachments: p, has_more: more } });
  },
};

test('Library pages in with stable cursors', async () => {
  route(libraryRoutes);
  server.posts = posts(120, 1);
  server.photos = photos(130, 1);
  set({ library: null });
  await loadLibrary();
  assert.equal(S.library.saved.length, 50);
  assert.equal(S.library.photos.length, 60);
  await loadMoreSaved(); await loadMoreSaved();
  await loadMorePhotos(); await loadMorePhotos();
  assert.equal(new Set(S.library.saved.map((p) => p.id)).size, 120);
  assert.equal(S.library.savedMore, false);
  assert.equal(new Set(S.library.photos.map((p) => p.id)).size, 130, 'ties on used_at handled by the id cursor');
  assert.equal(S.library.photosMore, false);
});

test('a Library refresh reloads as far as was shown and revalidates membership', async () => {
  route(libraryRoutes);
  server.posts = posts(120, 1);
  set({ library: null });
  await loadLibrary();
  await loadMoreSaved(); // 100 shown
  server.posts.find((p) => p.id === 'p30').liked = false; // unliked on another device (page 2)
  server.posts.push({ id: 'old', created_at: 45.5, liked: true }); // an older post liked elsewhere
  server.posts.sort((a, b) => b.created_at - a.created_at);
  server.posts.unshift(...posts(130, 121)); // 10 new
  await loadLibrary();
  const ids = S.library.saved.map((p) => p.id);
  assert.ok(ids.length >= 100, 'pages already shown stay shown');
  assert.ok(!ids.includes('p30'), 'unliked elsewhere: gone');
  assert.ok(ids.includes('old'), 'liked elsewhere: shown');
  assert.ok(ids.includes('p130') && ids.includes('p121'), 'new ones on top');
  assert.equal(S.library.savedMore, ids.length < server.posts.filter((p) => p.liked).length, 'has_more matches what is left');
  for (let i = 1; i < ids.length; i++) {
    assert.ok(S.library.saved[i - 1].created_at > S.library.saved[i].created_at, 'contiguous, newest first');
  }
});

test('a page that comes back after the list was replaced is dropped', async () => {
  route(libraryRoutes);
  server.posts = posts(100, 1);
  set({ library: null });
  await loadLibrary(); // 100..51
  let open;
  gate = new Promise((r) => { open = r; });
  const more = loadMoreSaved(); // cursor p51, waits
  await tick();
  server.posts = posts(200, 1); // 100 new posts arrived
  await loadLibrary(); // first page is now 200..151
  open(); gate = null;
  await more;
  const ids = S.library.saved.map((p) => p.id);
  assert.equal(ids[0], 'p200');
  assert.ok(!ids.includes('p50'), 'the stale page (50..1) did not join 200..151');
  assert.equal(S.library.savedMore, true);
});

// ---------------------------------------------------------------- sign-in

test('a sign-in response never regresses state that events advanced', () => {
  set({ logins: {} });
  const base = { id: 's1', harness: 'claude', started_at: 10 };
  const mark = loginMark('claude');
  handleEvent({ type: 'harness.login', login: { ...base, state: 'waiting_for_code', url: 'https://example.com/a' } });
  applyLoginResponse('claude', { ...base, state: 'starting' }, mark);
  assert.equal(S.logins.claude.state, 'waiting_for_code');
  // No event since the request: the response applies.
  const mark2 = loginMark('claude');
  applyLoginResponse('claude', { ...base, state: 'verifying' }, mark2);
  assert.equal(S.logins.claude.state, 'verifying');
  // Events may go back (a rejected code) and are applied as they come.
  handleEvent({ type: 'harness.login', login: { ...base, state: 'waiting_for_code', error: 'rejected' } });
  assert.equal(S.logins.claude.state, 'waiting_for_code');
  // A response or event for an earlier session is stale.
  applyLoginResponse('claude', { id: 's0', harness: 'claude', started_at: 5, state: 'cancelled' }, loginMark('claude'));
  handleEvent({ type: 'harness.login', login: { id: 's0', harness: 'claude', started_at: 5, state: 'failed' } });
  assert.equal(S.logins.claude.id, 's1');
  // A newer session replaces it.
  applyLoginResponse('claude', { id: 's2', harness: 'claude', started_at: 20, state: 'starting' }, loginMark('claude'));
  assert.equal(S.logins.claude.id, 's2');
});

test('a finished sign-in is dropped when the CLI is signed out again', () => {
  set({ harnesses: [{ name: 'codex', auth: 'ok' }], logins: { codex: { id: 'x', harness: 'codex', state: 'connected' } } });
  handleEvent({ type: 'harness.status', harness: { name: 'codex', auth: 'unknown' } });
  assert.equal(S.logins.codex?.state, 'connected', 'unknown counts as usable');
  handleEvent({ type: 'harness.status', harness: { name: 'codex', auth: 'auth_required' } });
  assert.equal(S.logins.codex, undefined);
});
