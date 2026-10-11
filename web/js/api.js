// Iris REST client and WebSocket. Talks only to the documented API
// (docs/API.md). The device token lives in localStorage on this device.

const TOKEN_KEY = 'iris.token';
let token = localStorage.getItem(TOKEN_KEY) || '';
let unauthorizedHandler = () => {};

export const hasToken = () => !!token;
export function setToken(t) {
  token = t || '';
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

/** Call `/api<path>`. JSON bodies are serialized; Blobs are sent raw. */
export async function api(method, path, body, { raw = false, timeout } = {}) {
  const headers = {};
  if (token) headers.Authorization = 'Bearer ' + token;
  let payload;
  if (body instanceof Blob || body instanceof ArrayBuffer) payload = body;
  else if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  }
  // Uploads may take a while on cellular; everything else should be quick.
  const ms = timeout ?? (payload instanceof Blob ? 120000 : 20000);
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), ms);
  let res;
  try {
    res = await fetch('/api' + path, { method, headers, body: payload, signal: ctl.signal });
  } catch {
    throw new ApiError(0, 'offline', "I can't reach your Iris server right now.");
  } finally {
    clearTimeout(timer);
  }
  if (res.status === 401 && path !== '/pair') unauthorizedHandler();
  if (!res.ok) {
    let code = 'error';
    let message = res.statusText || 'Something went wrong';
    try {
      const j = await res.json();
      code = j.error?.code || code;
      message = j.error?.message || message;
    } catch { /* not JSON */ }
    throw new ApiError(res.status, code, message);
  }
  if (raw) return res;
  const ct = res.headers.get('content-type') || '';
  return ct.includes('json') ? res.json() : res.text();
}

export const get = (p) => api('GET', p);
export const post = (p, b) => api('POST', p, b);
export const put = (p, b) => api('PUT', p, b);
export const patch = (p, b) => api('PATCH', p, b);
export const del = (p) => api('DELETE', p);

// Binary routes need the Authorization header, so images are fetched and
// shown through object URLs (cached per `/api/...` path).
const blobs = new Map();
export function blobUrl(path) {
  if (!blobs.has(path)) {
    const p = api('GET', path.replace(/^\/api(?=\/)/, ''), undefined, { raw: true })
      .then((r) => r.blob())
      .then((b) => URL.createObjectURL(b));
    p.catch(() => blobs.delete(path));
    blobs.set(path, p);
  }
  return blobs.get(path);
}

let live = null;
/** Send a message on the event stream (dropped while disconnected). */
export function wsSend(obj) {
  if (live && live.readyState === 1) live.send(JSON.stringify(obj));
}

/** Open the event stream; reconnects with backoff until `close()`. */
export function connectEvents(onEvent, onStatus) {
  let ws;
  let retry;
  let ping;
  let backoff = 1000;
  let stopped = false;
  const open = () => {
    clearTimeout(retry);
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    ws = new WebSocket(`${proto}://${location.host}/api/ws`);
    live = ws;
    ws.onopen = () => ws.send(JSON.stringify({ type: 'auth', token }));
    ws.onmessage = (e) => {
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
    ws.onclose = () => {
      clearInterval(ping);
      onStatus(false);
      if (stopped) return;
      retry = setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, 15000);
    };
    clearInterval(ping);
    ping = setInterval(() => ws.readyState === 1 && ws.send('{"type":"ping"}'), 25000);
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
      stopped = true;
      clearInterval(ping);
      clearTimeout(retry);
      document.removeEventListener('visibilitychange', wake);
      window.removeEventListener('online', wake);
      ws?.close();
    },
  };
}
