// web/js/api.js connectEvents: one live socket at a time.
import './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';

const listeners = {};
globalThis.document = {
  visibilityState: 'visible',
  addEventListener: (t, f) => { (listeners[t] ||= new Set()).add(f); },
  removeEventListener: (t, f) => listeners[t]?.delete(f),
};
window.addEventListener = (t, f) => { (listeners[t] ||= new Set()).add(f); };
window.removeEventListener = (t, f) => listeners[t]?.delete(f);
const fire = (t) => [...(listeners[t] || [])].forEach((f) => f());

const sockets = [];
globalThis.WebSocket = class {
  constructor(url) { this.url = url; this.readyState = 0; this.sent = []; sockets.push(this); }
  send(m) { this.sent.push(m); }
  close() { if (this.readyState < 2) this.readyState = 2; } // CLOSING; onclose comes later
  // test helpers
  opened() { this.readyState = 1; this.onopen?.(); }
  closed() { this.readyState = 3; this.onclose?.(); }
  deliver(m) { this.onmessage?.({ data: JSON.stringify(m) }); }
};

const { connectEvents } = await import('../../web/js/api.js');
const open = () => sockets.filter((s) => s.readyState < 2);

test('a wake while the socket is closing leaves exactly one live socket', async (t) => {
  sockets.length = 0;
  const events = [];
  const status = [];
  const conn = connectEvents((m) => events.push(m.type), (on) => status.push(on));
  t.after(() => conn.close()); // stop timers even if an assertion fails
  const first = sockets[0];
  first.opened();
  first.deliver({ type: 'ready' });
  first.close(); // network drop: CLOSING
  fire('online'); // wake while CLOSING opens a replacement
  assert.equal(sockets.length, 2);
  const second = sockets[1];
  second.opened();
  second.deliver({ type: 'ready' });
  first.closed(); // the old socket's close arrives late
  await new Promise((r) => setTimeout(r, 1200)); // past the first reconnect backoff
  assert.equal(sockets.length, 2, 'the old socket did not schedule another reconnect');
  assert.deepEqual(open(), [second]);
  first.deliver({ type: 'message.delta' }); // a straggler from the old socket
  second.deliver({ type: 'message.delta' });
  assert.equal(events.filter((t) => t === 'message.delta').length, 1, 'no duplicate events');
  assert.equal(status.at(-1), true, 'still online on the live socket');
  conn.close();
  assert.deepEqual(open(), [], 'close() closes the live socket');
  assert.equal(status.at(-1), false);
  second.closed();
  await new Promise((r) => setTimeout(r, 1200));
  assert.equal(sockets.length, 2, 'no reconnect after close()');
  fire('online');
  assert.equal(sockets.length, 2, 'wake listeners removed');
});

test('the socket says away in the background and present when back', async (t) => {
  sockets.length = 0;
  document.visibilityState = 'visible';
  const conn = connectEvents(() => {}, () => {});
  t.after(() => { conn.close(); document.visibilityState = 'visible'; });
  const said = (s) => s.sent.map((m) => JSON.parse(m).type);
  const first = sockets[0];
  document.visibilityState = 'hidden';
  fire('visibilitychange'); // before the socket opens: nothing to tell
  first.opened();
  assert.deepEqual(said(first), ['auth'], 'nothing goes before auth');
  first.deliver({ type: 'ready' });
  assert.deepEqual(said(first), ['auth', 'away'], 'a socket opened in the background says so');
  document.visibilityState = 'visible';
  fire('visibilitychange');
  document.visibilityState = 'hidden';
  fire('visibilitychange');
  fire('pagehide');
  assert.deepEqual(said(first), ['auth', 'away', 'present', 'away', 'away']);
  document.visibilityState = 'visible';
  fire('pageshow');
  assert.equal(said(first).at(-1), 'present');
  // The phone dropped the socket while away; back on screen it reconnects
  // and reports present after ready.
  first.closed();
  fire('visibilitychange');
  const second = sockets[1];
  assert.ok(second, 'reconnected on wake');
  second.opened();
  second.deliver({ type: 'ready' });
  assert.deepEqual(said(second), ['auth', 'present']);
  conn.close();
  fire('pagehide');
  fire('visibilitychange');
  assert.deepEqual(said(second), ['auth', 'present'], 'listeners removed on close');
});
