// App state: one plain object, a change counter, and loaders that keep it
// in sync with the REST API and the WebSocket event stream.
import { useEffect, useReducer, useRef } from './h.js';
import { get, post, patch, seg, isId } from './api.js';
import { undos, withoutMemory } from './memory-text.js';

/** Fresh client state: everything here belongs to the signed-in user. */
function initialState() {
  return {
    me: null,            // GET /api/me
    settings: null,      // Settings
    harnesses: [],       // HarnessStatus[]
    online: false,       // WebSocket connected
    convs: null,         // Conversation[] (not archived)
    archived: null,      // archived Conversation[] (loaded on demand)
    msgs: {},            // conversation id -> {list, hasMore, loading}
    live: {},            // assistant message id -> {text, acts: Activity[], queued, cid}
    logins: {},          // harness -> LoginStatus (owner)
    engines: null,       // GET /api/engines (owner)
    connectors: null,    // GET /api/connectors (owner)
    slots: null,         // Slot[]
    feed: null,          // {posts, hasMore, status}
    ideas: null,
    goals: null,
    library: null,
    memory: null,        // {items, more}: the owner's notes (Memory screen)
    route: parseRoute(),
    drawer: false,
    story: null,         // {topic index, story index}
    sheet: null,         // bottom sheet render function
    toast: null,
    toastAction: null,   // {label, run} shown in the toast (e.g. Undo)
    drafts: {},          // conversation id -> composer text
    browser: null,       // GET /api/browser (owner): Iris's computer
    browsing: {},        // assistant message id -> { cid, until }: while its turn uses the browser (until null), then lingering until `until` (ms)
  };
}

export const S = initialState();

// Live view frames skip the store (no app-wide re-render per frame).
export const frames = { subs: new Set(), onReady: new Set() };

const subs = new Set();
let pending = false;
let version = 0;

/** Merge a patch (or a function returning one) and re-render. */
export function set(patch) {
  Object.assign(S, typeof patch === 'function' ? patch(S) : patch);
  version++;
  if (!pending) {
    pending = true;
    queueMicrotask(() => {
      pending = false;
      subs.forEach((f) => f());
    });
  }
}

/**
 * Forget everything about the signed-in user (revoked device, re-pair):
 * data, drafts, open sheets and overlays, and any ad hoc keys.
 */
export function resetUserState() {
  connectorsLoading = null;
  connectorsTried = false;
  userGen++;
  const fresh = initialState();
  for (const k of Object.keys(S)) if (!(k in fresh)) delete S[k];
  set(fresh);
}

/** Re-render the calling component whenever the store changes. */
export function useStore() {
  const [, force] = useReducer((x) => x + 1, 0);
  const seen = useRef(version);
  seen.current = version;
  useEffect(() => {
    subs.add(force);
    // A change between render and subscribing would otherwise be missed.
    if (seen.current !== version) force();
    return () => subs.delete(force);
  }, []);
  return S;
}

// ------------------------------------------------------------------ routes

// A malformed escape (`%E0`) must not throw while parsing the route.
// (A declaration: `S` above calls parseRoute before consts initialize.)
function decodeSeg(p) {
  try { return decodeURIComponent(p); } catch { return p; }
}

export function parseRoute() {
  const raw = location.hash.replace(/^#\/?/, '');
  const [path, query = ''] = raw.split('?');
  const parts = path.split('/').filter(Boolean).map(decodeSeg);
  return { parts, query: new URLSearchParams(query), tab: parts[0] || 'chat' };
}

// replace: swap the current screen instead of adding one, so leaving a
// flow (like the Google setup wizard) can't be undone with Back into it.
export function go(path, replace = false) {
  const next = '#/' + path.replace(/^\/+/, '');
  if (location.hash !== next) {
    if (replace) location.replace(location.pathname + location.search + next);
    else location.hash = next;
  }
  set({ drawer: false });
}

// The screen one layer up from a route, like a file browser's Up: Back
// always climbs toward the top (and out of Settings), never replays history.
export function parentRoute(hash) {
  const raw = hash.replace(/^#\/?/, '');
  const [path, query = ''] = raw.split('?');
  const parts = path.split('/').filter(Boolean);
  const q = new URLSearchParams(query);
  // A screen opened from a chat (like the browser) closes back to it.
  const from = q.get('return');
  if (from && /^chat(\/|$)/.test(from)) return from;
  // A view of the same screen (?view=…, ?edit=…) sits one layer above it.
  if (query && !(parts[1] === 'files' && q.has('edit'))) return parts.join('/') || 'chat';
  const [tab, sub, a] = parts;
  if (tab === 'iris') {
    if (!sub) return 'chat';
    if (sub === 'connections' && (a === 'add' || a === 'group' || a === 'google-help' || a === 'new' || a === 'trusted')) return 'iris/connections';
    if (sub === 'files' && a === 'system') return 'iris/files';
    if (parts.length > 2) return parts.slice(0, -1).join('/');
    return 'iris';
  }
  if (tab === 'settings') return sub ? 'settings' : 'iris';
  if (tab === 'chat') return 'chat';
  return parts.length > 1 ? parts.slice(0, -1).join('/') : 'chat';
}

export function back() {
  go(parentRoute(location.hash), true);
}

window.addEventListener('hashchange', () => {
  set({ route: parseRoute(), drawer: false });
});

let toastTimer;
/** A short message; `action` ({label, run}) adds a button such as Undo. */
export function toast(text, action = null) {
  clearTimeout(toastTimer);
  set({ toast: text, toastAction: action });
  toastTimer = setTimeout(() => set({ toast: null, toastAction: null }), action ? 6000 : 3200);
}

export const isOwner = () => !!S.me?.user?.is_owner;
export const mainId = () => S.me?.main_conversation_id;

// ----------------------------------------------------------------- loaders

// conversation.created events that arrive while a list request is in flight
// may be newer than its answer: each running load collects them.
const convWatch = new Set();

export async function loadConvs() {
  const seen = [];
  convWatch.add(seen);
  try {
    const { conversations } = await get('/conversations');
    const ids = new Set(conversations.map((c) => c.id));
    set({ convs: [...conversations, ...seen.filter((c) => !ids.has(c.id))] });
  } finally {
    convWatch.delete(seen);
  }
}

export async function loadArchived() {
  const { conversations } = await get('/conversations?archived=true');
  set({ archived: conversations.filter((c) => c.archived_at) });
}

/** The conversation `cid`, also when it is archived (so not in S.convs). */
export function convFor(cid) {
  return S.convs?.find((c) => c.id === cid) || S.archived?.find((c) => c.id === cid);
}

/** A chat opened by URL that isn't in the list may be archived: look it up. */
const lookingUp = new Set();
export function lookUpConv(cid) {
  if (!S.convs || convFor(cid) || lookingUp.has(cid)) return;
  lookingUp.add(cid);
  get(`/conversations/${seg(cid)}`)
    .then(({ conversation: c }) => {
      if (c?.archived_at && !convFor(c.id)) set({ archived: [...(S.archived || []), c] });
    })
    .catch(() => {})
    .finally(() => lookingUp.delete(cid));
}

/** Archive or unarchive a chat and update both lists. */
export async function setArchived(c, archived) {
  const r = await patch(`/conversations/${seg(c.id)}`, { archived });
  const rest = (S.archived || []).filter((x) => x.id !== c.id);
  set({ archived: archived ? [r.conversation || { ...c, archived_at: Date.now() }, ...rest] : rest });
  await loadConvs();
}

// A reply only ever moves from streaming to a final status, and every
// change bumps updated_at. Keep whichever copy is further along (ties go to
// `a`).
const settled = (m) => (m.status === 'streaming' ? 0 : 1);
function newer(a, b) {
  if (settled(a) !== settled(b)) return settled(a) > settled(b) ? a : b;
  return (b.updated_at || 0) > (a.updated_at || 0) ? b : a;
}

/**
 * Apply the latest-page REST snapshot to the list we have now. Events may
 * have landed while the request was in flight: never regress a message the
 * events already moved on, and keep messages created after the snapshot.
 * Older pages we already loaded stay when they join up with the snapshot.
 * Returns the merged list and whether the snapshot joined the old list.
 */
function mergeSnapshot(list, snap) {
  const have = new Map(list.map((m) => [m.id, m]));
  const ids = new Set(snap.map((m) => m.id));
  let oldest = snap[0];
  for (const m of snap) if (m.created_at < oldest.created_at) oldest = m;
  const newest = snap.reduce((t, m) => Math.max(t, m.created_at), -Infinity);
  const joined = !!oldest && have.has(oldest.id);
  const out = snap.map((m) => (have.has(m.id) ? newer(m, have.get(m.id)) : m));
  for (const m of list) {
    if (ids.has(m.id)) continue;
    if (m.created_at >= newest) out.push(m);
    else if (joined && m.created_at <= oldest.created_at) {
      // Outside the latest page, so only a cached copy: it cannot be
      // confirmed as still streaming, and nothing would ever settle it.
      out.push(m.status === 'streaming' ? { ...m, status: 'interrupted' } : m);
    }
  }
  return { list: out.sort((x, y) => x.created_at - y.created_at), joined };
}

function patchBox(cid, patch) {
  set({ msgs: { ...S.msgs, [cid]: { ...(S.msgs[cid] || { list: [] }), ...patch } } });
}

/** End a load; a refresh asked for meanwhile runs now. */
function doneLoading(cid, patch) {
  const again = S.msgs[cid]?.stale;
  patchBox(cid, { ...patch, loading: false, stale: false });
  if (again) loadMessages(cid);
}

export async function loadMessages(cid) {
  if (S.msgs[cid]?.loading) {
    patchBox(cid, { stale: true });
    return;
  }
  patchBox(cid, { loading: true, stale: false });
  try {
    const r = await get(`/conversations/${seg(cid)}/messages?limit=60`);
    const cur = S.msgs[cid] || { list: [] };
    const { list, joined } = mergeSnapshot(cur.list, r.messages);
    const live = { ...S.live };
    for (const m of list) {
      if (m.status !== 'streaming') delete live[m.id];
      else if (!live[m.id]) live[m.id] = { text: '', acts: [], cid };
    }
    // A reply of this chat that is no longer streaming may also have left
    // the latest page (it finished while we were away): drop its entry too.
    const streaming = new Set(list.filter((m) => m.status === 'streaming').map((m) => m.id));
    for (const [mid, l] of Object.entries(live)) if (l.cid === cid && !streaming.has(mid)) delete live[mid];
    set({ live });
    doneLoading(cid, { list, hasMore: joined ? (cur.hasMore ?? r.has_more) : r.has_more, error: undefined });
  } catch (e) {
    doneLoading(cid, { error: e.message });
  }
}

export async function loadOlder(cid) {
  const cur = S.msgs[cid];
  if (!cur || !cur.hasMore || cur.loading || !cur.list.length) return;
  patchBox(cid, { loading: true });
  try {
    const r = await get(`/conversations/${seg(cid)}/messages?limit=60&before=${cur.list[0].created_at}`);
    const now = S.msgs[cid]?.list || [];
    const ids = new Set(now.map((m) => m.id));
    doneLoading(cid, { list: [...r.messages.filter((m) => !ids.has(m.id)), ...now], hasMore: r.has_more });
  } catch (e) {
    // Release the flag so paging and reconnect refreshes keep working.
    doneLoading(cid, {});
    throw e;
  }
}

// Lists that are refreshed whole (feed, ideas, goals, library) can have
// several refreshes and page loads in flight. Per list: `started` counts
// refreshes begun, `applied` is the newest one shown (or the point a local
// change made everything older stale), and `gen` changes whenever the list
// is replaced or changed here, which invalidates pages still loading.
const lists = {};
const track = (k) => (lists[k] ||= { started: 0, applied: 0, gen: 0 });

/** Begin a full refresh of list `k`; `apply(fn)` runs fn only if still newest. */
function refreshOf(k) {
  const t = track(k);
  const n = ++t.started;
  return (fn) => {
    if (n <= t.applied) return false;
    t.applied = n;
    t.gen++;
    fn();
    return true;
  };
}

/**
 * A local (optimistic) change to list `k`: refreshes already in flight may
 * carry the state from before it, and pages loading may no longer fit, so
 * both are dropped. Call before or with the change.
 */
export function touchList(k) {
  const t = track(k);
  t.applied = t.started;
  t.gen++;
}

/** A page was appended to list `k`: other pages in flight no longer fit. */
const grew = (k) => { track(k).gen++; };

/** For a page load: true while list `k` has not been replaced or changed since. */
function pageOf(k) {
  const t = track(k);
  const gen = t.gen;
  return () => t.gen === gen;
}

export async function loadFeed(more = false) {
  if (!more) {
    const apply = refreshOf('feed');
    const r = await get('/feed?limit=30');
    apply(() => set({ feed: { posts: r.posts, hasMore: r.has_more, status: r.status } }));
    return;
  }
  const last = S.feed?.posts?.at(-1);
  if (!last) return;
  const fits = pageOf('feed');
  const r = await get(`/feed?limit=30&before=${last.created_at}`);
  // This page only follows the list it was asked for.
  if (!fits() || S.feed?.posts.at(-1)?.id !== last.id) return;
  grew('feed');
  set({ feed: { posts: appendNew(S.feed.posts, r.posts), hasMore: r.has_more, status: r.status } });
}

/**
 * The Ideas screen shows open and started ideas; asking for just those means
 * the newest dismissed or done ones never crowd them out of the server's
 * page (at most 200, newest first).
 */
export async function loadIdeas() {
  const apply = refreshOf('ideas');
  const r = await get('/ideas?status=open,started');
  apply(() => set({ ideas: r.ideas }));
}
export async function loadGoals() {
  const apply = refreshOf('goals');
  const r = await get('/goals?include_done=true');
  apply(() => set({ goals: r.goals }));
}
const SAVED_PAGE = 50;
const PHOTO_PAGE = 60;
const ITEM_PAGE = 60;
const MAX_RELOAD = 600; // a refresh reloads at most this many shown items
// Photos page by (used_at, id), generated items by (created_at, id),
// saved posts by created_at; all newest first.
const photoCursor = (p) => `&before=${p.used_at}&before_id=${encodeURIComponent(p.id)}`;
const itemCursor = (it) => `&before=${it.created_at}&before_id=${encodeURIComponent(it.id)}`;

const appendNew = (list, more) => {
  const ids = new Set(list.map((x) => x.id));
  return [...list, ...more.filter((x) => !ids.has(x.id))];
};

// Library lists, newest first. `page(last)` fetches the page after `last`
// (the first page without it) and returns [items, hasMore].
const PAGED = {
  items: {
    more: 'itemsMore',
    page: (last) => get(`/library?limit=${ITEM_PAGE}${last ? itemCursor(last) : ''}`)
      .then((r) => [r.items, !!r.has_more]),
  },
  saved: {
    more: 'savedMore',
    page: (last) => get(`/feed?liked=true&limit=${SAVED_PAGE}${last ? `&before=${last.created_at}` : ''}`)
      .then((r) => [r.posts, !!r.has_more]),
  },
  photos: {
    more: 'photosMore',
    page: (last) => get(`/attachments?limit=${PHOTO_PAGE}${last ? photoCursor(last) : ''}`)
      .then((r) => [r.attachments, !!r.has_more]),
  },
};

/** Pages from the top until at least `want` items are loaded (or the end). */
async function loadPages(kind, want) {
  let list = [];
  let more;
  do {
    const [items, m] = await PAGED[kind].page(list.at(-1));
    const had = list.length;
    list = appendNew(list, items);
    more = m && list.length > had; // stop if a page brings nothing new
  } while (more && list.length < Math.min(want, MAX_RELOAD));
  return [list, more];
}
let connectorsLoading = null;
let userGen = 0;
/** Load the owner's connectors; concurrent callers share one request. */
export function loadConnectors() {
  if (!isOwner()) return Promise.resolve();
  const gen = userGen;
  const req = connectorsLoading || get('/connectors')
    // Dropped if the user signed out meanwhile.
    .then((r) => { if (gen === userGen) set({ connectors: r.connectors }); })
    .finally(() => { if (connectorsLoading === req) connectorsLoading = null; });
  connectorsLoading = req;
  return req;
}

let connectorsTried = false;
/** Load connectors once for cards that only need them for display. */
export function ensureConnectors() {
  if (S.connectors || connectorsTried || !isOwner()) return;
  connectorsTried = true;
  loadConnectors().catch(() => {});
}

/**
 * (Re)load the Library. Each list (made by me, photos, saved) is reloaded
 * from the server as far as it was shown, never patched from what was
 * cached, so an item unliked or removed elsewhere disappears and a newly
 * saved one shows up.
 */
export async function loadLibrary() {
  const apply = refreshOf('library');
  const cur = S.library;
  const [[items, itemsMore], [photos, photosMore], [saved, savedMore]] = await Promise.all([
    loadPages('items', cur?.items.length || 0),
    loadPages('photos', cur?.photos.length || 0),
    loadPages('saved', cur?.saved.length || 0),
  ]);
  apply(() => set({ library: { items, itemsMore, photos, photosMore, saved, savedMore } }));
}

/**
 * The next page of a Library list. Dropped if the list was replaced while
 * the request was out (its last item is no longer the cursor), so a page
 * never joins a list it doesn't follow.
 */
async function loadMore(kind) {
  const last = S.library?.[kind].at(-1);
  if (!last) return;
  const fits = pageOf('library');
  const [items, more] = await PAGED[kind].page(last);
  const lib = S.library;
  if (!fits() || !lib || lib[kind].at(-1)?.id !== last.id) return;
  grew('library');
  set({ library: { ...lib, [kind]: appendNew(lib[kind], items), [PAGED[kind].more]: more } });
}

/** Next page of Library's Saved posts. */
export const loadMoreSaved = () => loadMore('saved');
/** Next page of Library's generated items ("Made by me"). */
export const loadMoreItems = () => loadMore('items');
/** Next page of Library's Photos. */
export const loadMorePhotos = () => loadMore('photos');

export async function loadSlots() { set({ slots: (await get('/models/slots')).slots }); }
export async function loadBrowser() {
  if (!isOwner()) return;
  const { browser } = await get('/browser');
  setBrowser(browser);
}

function setBrowser(b) {
  const browser = { ...(S.browser || {}), ...b };
  const now = Date.now();
  const browsing = b.active ? Object.fromEntries(b.active.map((a) => [a.message_id,
    { cid: a.conversation_id, until: a.ended_at ? now + (a.linger_ms || 0) : null }])) : S.browsing;
  set({ browser, browsing });
}

// The Memory screen shows every current note (filtered on the phone), so
// switching topics is instant and the count is right. Paged by offset;
// a note that moves between pages while loading is shown once.
const MEMORY_PAGE = 100;
const MEMORY_MAX = 2000;
// A note that changes while pages load can move to a page already read.
// Changes announced meanwhile restart the load (a few times at most).
let memoryLoads = 0;
let memoryChanged = false;
export async function loadMemory() {
  if (!isOwner()) return;
  const apply = refreshOf('memory');
  memoryLoads++;
  try {
    let items;
    let more;
    for (let attempt = 0; attempt < 3; attempt++) {
      memoryChanged = false;
      items = [];
      const ids = new Set();
      more = true;
      let offset = 0;
      while (more && offset < MEMORY_MAX) {
        const r = await get(`/memory?limit=${MEMORY_PAGE}&offset=${offset}`);
        for (const i of r.items) if (!ids.has(i.id)) { ids.add(i.id); items.push(i); }
        more = !!r.has_more && r.items.length > 0;
        offset += r.items.length;
      }
      if (!memoryChanged) break;
    }
    apply(() => set({ memory: { items, more } }));
  } finally {
    memoryLoads--;
  }
}

/**
 * Forget everything happened (here or on another device): cached replies
 * lose their Noted lines and morning notes read as cleared, as on the server.
 */
export function memoryReset() {
  undos.clear();
  const msgs = {};
  for (const [cid, box] of Object.entries(S.msgs)) msgs[cid] = { ...box, list: box.list.map(withoutMemory) };
  set({ msgs });
  if (S.memory) loadMemory().catch(() => {});
}

/** A memory.noted event: show the Noted line under that reply. */
export function applyNoted(cid, mid, changes) {
  const cur = S.msgs[cid];
  const m = cur?.list.find((x) => x.id === mid);
  if (!m) return;
  upsert(cid, { ...m, meta: { ...(m.meta || {}), noted: changes } });
  set({});
}

export async function loadEngines() {
  if (!isOwner()) return;
  set({ engines: await get('/engines') });
}

const loaders = { feed: () => loadFeed(), ideas: loadIdeas, goals: loadGoals, library: loadLibrary, memory: loadMemory };

// ------------------------------------------------------------------ events

/** Whether the browser peek shows under assistant message `id` of a chat
 * (`list`, its messages in order): while its turn browses, and after the
 * turn at least until the entry's `until` and until a user message follows
 * it. */
export function peekShown(id, list, now = Date.now()) {
  const e = S.browsing[id];
  if (!e) return false;
  if (e.until == null || now < e.until) return true;
  const i = list.findIndex((x) => x.id === id);
  return !list.slice(i + 1).some((x) => x.role === 'user');
}

/** A message the server returned (also arrives as `message.updated`). */
export function upsertMessage(msg) {
  upsert(msg.conversation_id, msg);
  set({});
}

function upsert(cid, msg) {
  const cur = S.msgs[cid];
  if (!cur) return;
  const list = cur.list.slice();
  const i = list.findIndex((m) => m.id === msg.id);
  if (i >= 0) list[i] = msg;
  else {
    list.push(msg);
    list.sort((a, b) => a.created_at - b.created_at);
  }
  S.msgs = { ...S.msgs, [cid]: { ...cur, list } };
}

function bumpConv(cid, at) {
  if (!S.convs) return;
  if (!S.convs.some((c) => c.id === cid)) {
    loadConvs().catch(() => {});
    return;
  }
  S.convs = S.convs.map((c) => (c.id === cid ? { ...c, updated_at: at } : c));
}

function liveFor(mid, cid) {
  const l = S.live[mid] || { text: '', acts: [] };
  return cid && !l.cid ? { ...l, cid } : l;
}

/**
 * A finished sign-in ("connected") is stale once the CLI reports it is
 * signed out again (`auth_required`); drop it so the UI offers Reconnect.
 */
export function currentLogins(harnesses) {
  const logins = { ...S.logins };
  for (const h of harnesses || []) {
    if (h.auth === 'auth_required' && logins[h.name]?.state === 'connected') delete logins[h.name];
  }
  return logins;
}

// Sign-in sessions: events are authoritative and arrive in order (a
// rejected code legitimately goes back from verifying to waiting_for_code),
// so an HTTP response is applied only when no event for that harness came
// in while it was in flight. A session that started earlier than the one
// shown is stale either way.
const loginEvents = {}; // harness -> count of harness.login events seen

/** Call before a sign-in request; pass the result to applyLoginResponse. */
export const loginMark = (harness) => loginEvents[harness] || 0;

function newerSession(cur, next) {
  return !cur || cur.id === next.id || (next.started_at || 0) >= (cur.started_at || 0);
}

/** Apply a sign-in request's `{login}` response unless events moved past it. */
export function applyLoginResponse(harness, login, mark) {
  if (!login) return;
  const cur = S.logins[harness];
  if (cur && cur.id === login.id && loginMark(harness) !== mark) return;
  if (!newerSession(cur, login)) return;
  set({ logins: { ...S.logins, [harness]: login } });
}

export function handleEvent(ev) {
  switch (ev.type) {
    case 'ready':
      set({ settings: ev.settings, harnesses: ev.harnesses, logins: currentLogins(ev.harnesses) });
      refreshAll();
      frames.onReady.forEach((f) => f());
      break;
    case 'browser.frame':
      frames.subs.forEach((f) => f(ev));
      break;
    case 'browser.state':
      setBrowser(ev.browser);
      break;
    case 'browser.active':
      set({ browsing: { ...S.browsing, [ev.message_id]: { cid: ev.conversation_id, until: null } } });
      break;
    case 'browser.done': {
      // The peek stays a while (see `linger_ms`), or goes now.
      const browsing = { ...S.browsing };
      if (ev.linger_ms > 0) browsing[ev.message_id] = { cid: ev.conversation_id, until: Date.now() + ev.linger_ms };
      else delete browsing[ev.message_id];
      set({ browsing });
      break;
    }
    case 'browser.input_error':
      toast(ev.message);
      break;
    case 'resync':
      refreshAll();
      break;
    case 'message.created': {
      const m = ev.message;
      if (m.status === 'streaming' && !S.live[m.id]) S.live = { ...S.live, [m.id]: { text: '', acts: [], cid: ev.conversation_id } };
      upsert(ev.conversation_id, m);
      bumpConv(ev.conversation_id, m.created_at);
      set({});
      break;
    }
    case 'message.delta': {
      const l = liveFor(ev.message_id, ev.conversation_id);
      set({ live: { ...S.live, [ev.message_id]: { ...l, text: l.text + ev.text, queued: false } } });
      break;
    }
    case 'message.activity': {
      const l = liveFor(ev.message_id, ev.conversation_id);
      const acts = l.acts.filter((a) => a.id !== ev.activity.id);
      const i = l.acts.findIndex((a) => a.id === ev.activity.id);
      if (i >= 0) acts.splice(i, 0, ev.activity);
      else acts.push(ev.activity);
      set({ live: { ...S.live, [ev.message_id]: { ...l, acts, queued: false } } });
      break;
    }
    case 'turn.status': {
      const l = liveFor(ev.message_id, ev.conversation_id);
      set({ live: { ...S.live, [ev.message_id]: { ...l, queued: ev.status === 'queued' } } });
      break;
    }
    case 'message.completed': {
      upsert(ev.conversation_id, ev.message);
      const live = { ...S.live };
      delete live[ev.message.id];
      set({ live });
      break;
    }
    case 'message.updated':
      upsert(ev.conversation_id, ev.message);
      set({});
      break;
    case 'memory.reset':
      memoryReset();
      break;
    case 'memory.noted':
      applyNoted(ev.conversation_id, ev.message_id, ev.changes || []);
      break;
    case 'conversation.created':
      for (const seen of convWatch) seen.push(ev.conversation);
      if (S.convs && !S.convs.some((c) => c.id === ev.conversation.id)) {
        set({ convs: [...S.convs, ev.conversation] });
      }
      break;
    case 'entities.changed':
      if (ev.kinds.includes('memory') && memoryLoads > 0) memoryChanged = true;
      for (const k of ev.kinds) if (S[k] && loaders[k]) loaders[k]().catch(() => {});
      if (ev.kinds.includes('feed') && S.library) loadLibrary().catch(() => {});
      break;
    case 'settings.changed':
      set({ settings: ev.settings });
      break;
    case 'harness.status':
      set({
        harnesses: S.harnesses.map((h) => (h.name === ev.harness.name ? { ...h, ...ev.harness, path: h.path } : h)),
        logins: currentLogins([ev.harness]),
      });
      break;
    case 'models.changed':
      set({ slots: ev.slots });
      break;
    case 'harness.login':
      loginEvents[ev.login.harness] = loginMark(ev.login.harness) + 1;
      if (newerSession(S.logins[ev.login.harness], ev.login)) set({ logins: { ...S.logins, [ev.login.harness]: ev.login } });
      break;
    case 'engines.changed':
      if (S.engines) set({ engines: { ...S.engines, engines: ev.engines } });
      break;
    case 'connectors.changed':
      if (S.connectors) loadConnectors().catch(() => {});
      break;
    default:
      break;
  }
}

/**
 * Live replies in chats that aren't loaded: reloading S.msgs won't see them
 * finish (their completion may have been missed while offline). Ask whether
 * a turn still holds each such chat and drop the entries if not.
 */
function reconcileLive() {
  const byChat = {};
  for (const [mid, l] of Object.entries(S.live)) {
    if (l.cid && S.msgs[l.cid]) continue; // loadMessages reconciles these
    (byChat[l.cid || ''] ||= []).push(mid);
  }
  for (const [cid, mids] of Object.entries(byChat)) {
    const drop = () => {
      const live = { ...S.live };
      for (const m of mids) delete live[m];
      set({ live });
    };
    if (!isId(cid)) { drop(); continue; }
    get(`/conversations/${seg(cid)}`)
      .then((r) => { if (!r.turn_running) drop(); })
      .catch((e) => { if (e.status === 403 || e.status === 404) drop(); });
  }
}

/** Refetch whatever is loaded (after a reconnect or a resync). */
export function refreshAll() {
  loadConvs().catch(() => {});
  for (const cid of Object.keys(S.msgs)) loadMessages(cid).catch(() => {});
  reconcileLive();
  for (const k of Object.keys(loaders)) if (S[k]) loaders[k]().catch(() => {});
  if (S.slots) loadSlots().catch(() => {});
  if (S.engines) loadEngines().catch(() => {});
  loadBrowser().catch(() => {});
  if (S.connectors) loadConnectors().catch(() => {});
}

/** Send a message (text and/or attachment ids) and track the reply. */
export async function sendMessage(cid, text, attachments = []) {
  const r = await post(`/conversations/${seg(cid)}/messages`, { text, attachments });
  upsert(cid, r.user_message);
  // The events can beat the HTTP response; never regress a newer state.
  const known = S.msgs[cid]?.list.find((m) => m.id === r.assistant_message.id);
  if (!known) {
    upsert(cid, r.assistant_message);
    if (!S.live[r.assistant_message.id]) {
      S.live = { ...S.live, [r.assistant_message.id]: { text: '', acts: [], cid } };
    }
  }
  bumpConv(cid, r.user_message.created_at);
  set({});
  return r;
}
