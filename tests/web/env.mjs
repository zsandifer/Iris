// Browser-side tests for the app in web/js/, run with node's own test
// runner (no packages): `node --test 'tests/web/*.test.mjs'`.
//
// Minimal browser globals for importing those modules in node. Import this
// first: api.js reads localStorage and store.js reads location at import.

const memoryStorage = () => {
  const m = new Map();
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => m.set(k, String(v)),
    removeItem: (k) => m.delete(k),
    clear: () => m.clear(),
  };
};

globalThis.localStorage = memoryStorage();
globalThis.sessionStorage = memoryStorage();
globalThis.location = { hash: '', protocol: 'http:', host: 'iris.test', href: 'http://iris.test/' };
globalThis.window = globalThis.window || { addEventListener() {}, removeEventListener() {} };
globalThis.history = { length: 1, back() {}, replaceState() {} };

// fetch: every test routes requests through `onFetch` (url, opts) => Response-like.
let handler = () => { throw new TypeError('no fetch handler set'); };
export const calls = [];
export function onFetch(fn) { handler = fn; }
globalThis.fetch = async (url, opts = {}) => {
  calls.push({ url, method: opts.method || 'GET', auth: opts.headers?.Authorization, body: opts.body });
  return handler(url, opts);
};

/**
 * A Response-like object. `body` is returned by json()/text()/blob(); with
 * `hold: true` the body only resolves when `release()` is called, and
 * rejects with an AbortError if the request's signal aborts (as browsers do).
 */
export function reply(opts, { status = 200, type = 'application/json', body = {}, hold = false } = {}) {
  let release;
  const ready = hold ? new Promise((resolve, reject) => {
    release = resolve;
    opts.signal?.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
  }) : Promise.resolve();
  const read = (f) => () => ready.then(f);
  const res = {
    ok: status >= 200 && status < 300,
    status,
    statusText: '',
    headers: { get: (h) => (h.toLowerCase() === 'content-type' ? type : null) },
    json: read(() => (typeof body === 'string' ? JSON.parse(body) : body)),
    text: read(() => (typeof body === 'string' ? body : JSON.stringify(body))),
    blob: read(() => new Blob([typeof body === 'string' ? body : JSON.stringify(body)], { type })),
  };
  res.release = () => release?.();
  return res;
}

export const tick = () => new Promise((r) => setTimeout(r, 0));
