// App state: one plain object, a change counter, and loaders that keep it
// in sync with the REST API and the WebSocket event stream.
import { useEffect, useReducer, useRef } from './h.js';
import { get, post, patch, seg } from './api.js';

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
    live: {},            // assistant message id -> {text, acts: Activity[], queued}
    logins: {},          // harness -> LoginStatus (owner)
    engines: null,       // GET /api/engines (owner)
    connectors: null,    // GET /api/connectors (owner)
    slots: null,         // Slot[]
    feed: null,          // {posts, hasMore}
    ideas: null,
    goals: null,
    library: null,
    route: parseRoute(),
    drawer: false,
    story: null,         // {topic index, story index}
    sheet: null,         // bottom sheet render function
    toast: null,
    drafts: {},          // conversation id -> composer text
  };
}

export const S = initialState();

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

export function go(path) {
  const next = '#/' + path.replace(/^\/+/, '');
  if (location.hash !== next) location.hash = next;
  set({ drawer: false });
}

export function back(fallback = 'chat') {
  if (history.length > 1 && sessionStorage.getItem('iris.nav')) history.back();
  else go(fallback);
}

window.addEventListener('hashchange', () => {
  sessionStorage.setItem('iris.nav', '1');
  set({ route: parseRoute(), drawer: false });
});

let toastTimer;
export function toast(text) {
  clearTimeout(toastTimer);
  set({ toast: text });
  toastTimer = setTimeout(() => set({ toast: null }), 3200);
}

export const isOwner = () => !!S.me?.user?.is_owner;
export const mainId = () => S.me?.main_conversation_id;

// ----------------------------------------------------------------- loaders

export async function loadConvs() {
  const { conversations } = await get('/conversations');
  set({ convs: conversations });
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
    if (m.created_at >= newest || (joined && m.created_at <= oldest.created_at)) out.push(m);
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
      else if (!live[m.id]) live[m.id] = { text: '', acts: [] };
    }
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

export async function loadFeed(more = false) {
  const before = more && S.feed?.posts?.length ? `&before=${S.feed.posts.at(-1).created_at}` : '';
  const r = await get(`/feed?limit=30${before}`);
  const posts = more ? [...(S.feed?.posts || []), ...r.posts] : r.posts;
  set({ feed: { posts, hasMore: r.has_more } });
}
export async function loadIdeas() { set({ ideas: (await get('/ideas')).ideas }); }
export async function loadGoals() { set({ goals: (await get('/goals?include_done=true')).goals }); }
const SAVED_PAGE = 50;
const PHOTO_PAGE = 60;
const MAX_RELOAD = 600; // a refresh reloads at most this many shown items
const photoCursor = (p) => `&before=${p.used_at}&before_id=${encodeURIComponent(p.id)}`;

const appendNew = (list, more) => {
  const ids = new Set(list.map((x) => x.id));
  return [...list, ...more.filter((x) => !ids.has(x.id))];
};

// Library lists, newest first. `page(last)` fetches the page after `last`
// (the first page without it) and returns [items, hasMore].
const PAGED = {
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
 * (Re)load the Library. Saved and Photos are reloaded from the server as
 * far as they were shown, never patched from what was cached, so an item
 * unliked or removed elsewhere disappears and a newly saved one shows up.
 */
export async function loadLibrary() {
  const cur = S.library;
  const [items, [photos, photosMore], [saved, savedMore]] = await Promise.all([
    get('/library').then((r) => r.items),
    loadPages('photos', cur?.photos.length || 0),
    loadPages('saved', cur?.saved.length || 0),
  ]);
  set({ library: { items, photos, photosMore, saved, savedMore } });
}

/**
 * The next page of a Library list. Dropped if the list was replaced while
 * the request was out (its last item is no longer the cursor), so a page
 * never joins a list it doesn't follow.
 */
async function loadMore(kind) {
  const last = S.library?.[kind].at(-1);
  if (!last) return;
  const [items, more] = await PAGED[kind].page(last);
  const lib = S.library;
  if (!lib || lib[kind].at(-1)?.id !== last.id) return;
  set({ library: { ...lib, [kind]: appendNew(lib[kind], items), [PAGED[kind].more]: more } });
}

/** Next page of Library's Saved posts. */
export const loadMoreSaved = () => loadMore('saved');
/** Next page of Library's Photos. */
export const loadMorePhotos = () => loadMore('photos');

export async function loadSlots() { set({ slots: (await get('/models/slots')).slots }); }
export async function loadEngines() {
  if (!isOwner()) return;
  set({ engines: await get('/engines') });
}

const loaders = { feed: () => loadFeed(), ideas: loadIdeas, goals: loadGoals, library: loadLibrary };

// ------------------------------------------------------------------ events

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

function liveFor(mid) {
  return S.live[mid] || { text: '', acts: [] };
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
      break;
    case 'resync':
      refreshAll();
      break;
    case 'message.created': {
      const m = ev.message;
      if (m.status === 'streaming' && !S.live[m.id]) S.live = { ...S.live, [m.id]: { text: '', acts: [] } };
      upsert(ev.conversation_id, m);
      bumpConv(ev.conversation_id, m.created_at);
      set({});
      break;
    }
    case 'message.delta': {
      const l = liveFor(ev.message_id);
      set({ live: { ...S.live, [ev.message_id]: { ...l, text: l.text + ev.text, queued: false } } });
      break;
    }
    case 'message.activity': {
      const l = liveFor(ev.message_id);
      const acts = l.acts.filter((a) => a.id !== ev.activity.id);
      const i = l.acts.findIndex((a) => a.id === ev.activity.id);
      if (i >= 0) acts.splice(i, 0, ev.activity);
      else acts.push(ev.activity);
      set({ live: { ...S.live, [ev.message_id]: { ...l, acts, queued: false } } });
      break;
    }
    case 'turn.status': {
      const l = liveFor(ev.message_id);
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
    case 'conversation.created':
      if (S.convs && !S.convs.some((c) => c.id === ev.conversation.id)) {
        set({ convs: [...S.convs, ev.conversation] });
      }
      break;
    case 'entities.changed':
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

/** Refetch whatever is loaded (after a reconnect or a resync). */
export function refreshAll() {
  loadConvs().catch(() => {});
  for (const cid of Object.keys(S.msgs)) loadMessages(cid).catch(() => {});
  for (const k of Object.keys(loaders)) if (S[k]) loaders[k]().catch(() => {});
  if (S.slots) loadSlots().catch(() => {});
  if (S.engines) loadEngines().catch(() => {});
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
      S.live = { ...S.live, [r.assistant_message.id]: { text: '', acts: [] } };
    }
  }
  bumpConv(cid, r.user_message.created_at);
  set({});
  return r;
}
