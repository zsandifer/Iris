// Iris REST client and WebSocket. Talks only to the documented API
// (docs/API.md). The device token lives in localStorage on this device.

const TOKEN_KEY = 'iris.token';
let token = localStorage.getItem(TOKEN_KEY) || '';
let unauthorizedHandler = () => {};
// Bumped whenever the token changes. Responses to requests made under an
// earlier token are dropped, so one user's data never reaches the next.
let generation = 0;

export const hasToken = () => !!token;
export function setToken(t) {
  const next = t || '';
  if (next !== token) {
    generation++;
    clearBlobs();
  }
  token = next;
  if (token) localStorage.setItem(TOKEN_KEY, token);
  else localStorage.removeItem(TOKEN_KEY);
}
export function onUnauthorized(fn) { unauthorizedHandler = fn; }

export class ApiError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

// Thrown for a response that belongs to an earlier sign-in. No message, so
// a caller that toasts errors shows nothing.
const stale = () => new ApiError(0, 'stale', '');

const offline = () => new ApiError(0, 'offline', "I can't reach your Iris server right now.");

/**
 * Call `/api<path>`. JSON bodies are serialized; Blobs are sent raw. The
 * response body is read here, as `as` ('blob' or 'text'; by default JSON
 * or text from the content type), so the deadline covers the whole body
 * and a body that finishes after the signed-in user changed is dropped.
 */
export async function api(method, path, body, { as, timeout } = {}) {
  const gen = generation;
  const headers = {};
  if (token) headers.Authorization = 'Bearer ' + token;
  let payload;
  if (body instanceof Blob || body instanceof ArrayBuffer) payload = body;
  else if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  }
  // Uploads and downloads may take a while on cellular; the rest is quick.
  const ms = timeout ?? (payload instanceof Blob || as ? 120000 : 20000);
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), ms);
  try {
    let res;
    try {
      res = await fetch('/api' + path, { method, headers, body: payload, signal: ctl.signal });
    } catch {
      throw offline();
    }
    if (gen !== generation) throw stale();
    if (res.status === 401 && path !== '/pair') unauthorizedHandler();
    if (!res.ok) {
      let code = 'error';
      let message = res.statusText || 'Something went wrong';
      try {
        const j = await res.json();
        code = j.error?.code || code;
        message = j.error?.message || message;
      } catch { /* not JSON, or the body stalled */ }
      if (gen !== generation) throw stale();
      throw new ApiError(res.status, code, message);
    }
    const kind = as || ((res.headers.get('content-type') || '').includes('json') ? 'json' : 'text');
    let data;
    try {
      data = await res[kind]();
    } catch (e) {
      if (gen !== generation) throw stale();
      if (ctl.signal.aborted || e?.name === 'AbortError' || e?.name === 'TypeError') throw offline();
      throw e;
    }
    if (gen !== generation) throw stale();
    return data;
  } finally {
    clearTimeout(timer);
  }
}

export const get = (p) => api('GET', p);
export const post = (p, b) => api('POST', p, b);
export const put = (p, b) => api('PUT', p, b);
export const patch = (p, b) => api('PATCH', p, b);
export const del = (p) => api('DELETE', p);

/** Server ids: 32 lowercase hex characters (`util::new_id`). */
export const isId = (s) => typeof s === 'string' && /^[0-9a-f]{32}$/.test(s);

/**
 * One URL path segment for an API path. Encodes `/`, `?`, `#`, and `%`, and
 * refuses empty and dot segments (fetch would resolve them as `..`).
 */
export function seg(s) {
  const v = String(s ?? '');
  if (!v || v === '.' || v === '..') throw new ApiError(400, 'bad_path', "That link doesn't look right.");
  return encodeURIComponent(v);
}

// Binary routes need the Authorization header, so images are fetched and
// shown through object URLs. Entries in use (held) are never revoked; up to
// BLOB_CACHE unused ones stay around for quick re-display, oldest evicted
// and revoked first.
const BLOB_CACHE = 40;
const blobs = new Map(); // `/api/...` path -> {p: Promise<object URL>, refs}

function trimBlobs() {
  for (const [k, e] of blobs) {
    if (blobs.size <= BLOB_CACHE) break;
    if (e.refs > 0) continue;
    blobs.delete(k);
    e.p.then((u) => URL.revokeObjectURL(u), () => {});
  }
}

/** Revoke and forget every cached image (the signed-in user changed). */
function clearBlobs() {
  for (const e of blobs.values()) e.p.then((u) => URL.revokeObjectURL(u), () => {});
  blobs.clear();
}

/**
 * Hold an object URL for an authenticated `/api/...` path. The URL stays
 * valid until `release()` is called.
 */
export function holdBlob(path) {
  let e = blobs.get(path);
  if (e) blobs.delete(path); // re-insert as most recently used
  else {
    e = { refs: 0 };
    const gen = generation;
    e.p = api('GET', path.replace(/^\/api(?=\/)/, ''), undefined, { as: 'blob' })
      .then((b) => {
        if (gen !== generation) throw stale();
        return URL.createObjectURL(b);
      });
    e.p.catch(() => { if (blobs.get(path) === e) blobs.delete(path); });
  }
  blobs.set(path, e);
  e.refs++;
  trimBlobs();
  let held = true;
  return {
    url: e.p,
    release() {
      if (!held) return;
      held = false;
      e.refs--;
      trimBlobs();
    },
  };
}

/** Open the event stream; reconnects with backoff until `close()`. */
export function connectEvents(onEvent, onStatus) {
  let ws;
  let retry;
  let ping;
  let backoff = 1000;
  let stopped = false;
  // Only the newest socket is live. A replaced one (e.g. a wake while it was
  // still closing) gets no handlers, so it can neither deliver events twice
  // nor schedule another reconnect.
  const drop = (sock) => {
    if (!sock) return;
    sock.onopen = sock.onmessage = sock.onclose = sock.onerror = null;
    try { sock.close(); } catch { /* already closed */ }
  };
  const open = () => {
    clearTimeout(retry);
    clearInterval(ping);
    drop(ws);
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const sock = new WebSocket(`${proto}://${location.host}/api/ws`);
    ws = sock;
    sock.onopen = () => sock.send(JSON.stringify({ type: 'auth', token }));
    sock.onmessage = (e) => {
      if (sock !== ws) return;
      let m;
      try { m = JSON.parse(e.data); } catch { return; }
      if (m.type === 'ready') {
        backoff = 1000;
        onStatus(true);
      }
      if (m.type === 'error' && m.code === 'unauthorized') {
        stopped = true;
        unauthorizedHandler();
        return;
      }
      onEvent(m);
    };
    sock.onclose = () => {
      if (sock !== ws) return;
      clearInterval(ping);
      onStatus(false);
      if (stopped) return;
      retry = setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
    ping = setInterval(() => sock.readyState === 1 && sock.send('{"type":"ping"}'), 25000);
  };
  const wake = () => {
    if (document.visibilityState === 'visible' && !stopped && ws && ws.readyState > 1) {
      backoff = 1000;
      open();
    }
  };
  document.addEventListener('visibilitychange', wake);
  window.addEventListener('online', wake);
  open();
  return {
    close() {
      if (stopped && !ws) return;
      stopped = true;
      clearInterval(ping);
      clearTimeout(retry);
      document.removeEventListener('visibilitychange', wake);
      window.removeEventListener('online', wake);
      drop(ws);
      ws = null;
      onStatus(false);
    },
  };
}
