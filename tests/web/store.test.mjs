// store.js: message snapshots vs live events, paging, Library reloads,
// sign-in state.
import { onFetch, reply, tick } from './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { setToken } from '../../web/js/api.js';
import {
  S, set, loadMessages, loadOlder, handleEvent, loadLibrary, loadMoreSaved, loadMorePhotos, loadMoreItems,
  loadFeed, refreshAll,
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

const server = { posts: [], photos: [], items: [] };
const posts = (hi, lo) => { const a = []; for (let i = hi; i >= lo; i--) a.push({ id: 'p' + i, created_at: i, liked: true }); return a; };
const pid = (i) => String(i).padStart(4, '0');
const photos = (hi, lo) => {
  const a = [];
  for (let i = hi; i >= lo; i--) a.push({ id: pid(i), used_at: Math.floor(i / 2) });
  return a.sort((x, y) => y.used_at - x.used_at || (x.id < y.id ? 1 : -1));
};
let gate = null; // when set, page requests wait for it
const page = (list, q, key = 'used_at') => {
  const lim = +q.get('limit');
  let l = list;
  if (q.get('before_id')) {
    const b = +q.get('before'); const id = q.get('before_id');
    l = l.filter((p) => p[key] < b || (p[key] === b && p.id < id));
  } else if (q.get('before')) l = l.filter((p) => p.created_at < +q.get('before'));
  return [l.slice(0, lim), l.length > lim];
};
const libraryRoutes = {
  '/api/library': async (q, opts) => {
    if (gate && q.get('before')) await gate;
    const [p, more] = page(server.items, q, 'created_at');
    return reply(opts, { body: { items: p, has_more: more } });
  },
  '/api/feed': async (q, opts) => {
    if (gate && q.get('before')) await gate;
    const [p, more] = page(q.get('liked') === 'true' ? server.posts.filter((x) => x.liked) : server.posts, q);
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

// Generated items ("Made by me") page by (created_at, id), ties included.
const items = (hi, lo) => {
  const a = [];
  for (let i = hi; i >= lo; i--) a.push({ id: 'i' + String(i).padStart(4, '0'), created_at: Math.floor(i / 3), mime: 'text/markdown' });
  return a.sort((x, y) => y.created_at - x.created_at || (x.id < y.id ? 1 : -1));
};

test('generated items page with a (created_at, id) cursor and reload as far as shown', async () => {
  route(libraryRoutes);
  server.items = items(150, 1);
  set({ library: null });
  await loadLibrary();
  assert.equal(S.library.items.length, 60);
  assert.equal(S.library.itemsMore, true);
  await loadMoreItems();
  await loadMoreItems();
  assert.equal(new Set(S.library.items.map((x) => x.id)).size, 150, 'every item once, ties on created_at included');
  assert.equal(S.library.itemsMore, false);
  // Removed elsewhere, and a new one: a refresh reloads all 150 shown.
  server.items = items(151, 1).filter((x) => x.id !== 'i0010');
  await loadLibrary();
  const ids = S.library.items.map((x) => x.id);
  assert.equal(ids.length, 150);
  assert.ok(ids.includes('i0151') && !ids.includes('i0010'));
  assert.equal(S.library.itemsMore, false);
});

test('a generated-items page that comes back after the list was replaced is dropped', async () => {
  route(libraryRoutes);
  server.items = items(100, 1);
  set({ library: null });
  await loadLibrary(); // 60 shown
  let open;
  gate = new Promise((r) => { open = r; });
  const more = loadMoreItems();
  await tick();
  server.items = items(400, 1); // 300 new
  await loadLibrary(); // reloads 60: now 400..341
  open(); gate = null;
  await more;
  const ids = S.library.items.map((x) => x.id);
  assert.equal(ids.length, 60);
  assert.equal(ids[0], 'i0400');
  assert.equal(S.library.itemsMore, true);
});

// ------------------------------------------------------------------- feed

test('a feed page that comes back after a refresh replaced the list is dropped', async () => {
  route(libraryRoutes);
  server.posts = posts(100, 1);
  set({ feed: null });
  await loadFeed(); // 100..71
  let open;
  gate = new Promise((r) => { open = r; });
  const more = loadFeed(true); // cursor p71, waits
  await tick();
  server.posts = posts(200, 1);
  await loadFeed(); // refresh: 200..171
  open(); gate = null;
  await more;
  const ids = S.feed.posts.map((p) => p.id);
  assert.deepEqual([ids[0], ids.at(-1), ids.length], ['p200', 'p171', 30], 'the stale page (70..41) did not join');
  assert.equal(S.feed.hasMore, true);
  await loadFeed(true);
  assert.equal(S.feed.posts.at(-1).id, 'p141', 'paging continues from the new list');
});

// ------------------------------------------------------- live replies

test('after a reconnect, live replies in unopened chats are dropped once their turn is over', async () => {
  const A = 'b'.repeat(32); // not opened; its turn finished while offline
  const B = 'c'.repeat(32); // not opened; still running
  set({ live: {}, msgs: {} });
  handleEvent({ type: 'message.created', conversation_id: A, message: { ...msg('ra', 1, 'streaming'), conversation_id: A } });
  handleEvent({ type: 'message.created', conversation_id: B, message: { ...msg('rb', 1, 'streaming'), conversation_id: B } });
  handleEvent({ type: 'message.delta', conversation_id: B, message_id: 'rb2', text: 'x' }); // created by a delta
  assert.ok(S.live.ra && S.live.rb && S.live.rb2);
  route({
    '/api/conversations': (q, opts) => reply(opts, { body: { conversations: [] } }),
    [`/api/conversations/${A}`]: (q, opts) => reply(opts, { body: { conversation: { id: A }, turn_running: false } }),
    [`/api/conversations/${B}`]: (q, opts) => reply(opts, { body: { conversation: { id: B }, turn_running: true } }),
  });
  refreshAll();
  await tick(); await tick(); await tick();
  assert.equal(S.live.ra, undefined, 'finished turn: no longer shown as working');
  assert.ok(S.live.rb && S.live.rb2, 'running turn: kept');
});
