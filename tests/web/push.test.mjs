// web/js/push.js: what Settings shows must match what the server delivers.
import { onFetch, reply } from './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';

const KEY = 'BOrW0Kx1';
const OTHER_KEY = 'AAAAAAAA';
const bytes = (k) => Uint8Array.from(atob(k), (c) => c.charCodeAt(0));
let sub = null;
const subscription = (key, endpoint = 'https://fcm.googleapis.com/fcm/send/abc') => ({
  endpoint,
  options: { applicationServerKey: bytes(key).buffer },
  toJSON: () => ({ endpoint, keys: { p256dh: 'p', auth: 'a' } }),
  unsubscribe: async () => { sub = null; return true; },
});
const pushManager = {
  getSubscription: async () => sub,
  subscribe: async (o) => (sub = subscription(btoa(String.fromCharCode(...o.applicationServerKey)))),
};
globalThis.window.PushManager = function PushManager() {};
globalThis.Notification = window.Notification = { permission: 'granted', requestPermission: async () => 'granted' };
globalThis.matchMedia = () => ({ matches: false });
Object.defineProperty(globalThis, 'navigator', {
  configurable: true,
  value: {
    userAgent: 'test',
    platform: 'Linux',
    serviceWorker: { getRegistration: async () => ({ pushManager }), ready: Promise.resolve({ pushManager }) },
  },
});

const server = { enabled: true, key: KEY, reject: false, registered: [] };
onFetch((url, opts) => {
  if (url === '/api/push/vapid-public-key') {
    return reply(opts, { body: server.enabled ? { enabled: true, public_key: server.key } : { enabled: false, reason: 'not configured' } });
  }
  if (url === '/api/push/subscriptions') {
    server.registered.push(JSON.parse(opts.body).endpoint);
    return server.reject ? reply(opts, { status: 400, body: { error: { message: 'endpoint must be an https URL of a browser push service' } } })
      : reply(opts, { body: { subscription: {} } });
  }
  return reply(opts, { body: {} });
});

const { setToken } = await import('../../web/js/api.js');
const push = await import('../../web/js/push.js');
setToken('t');

test('after re-pairing, the browser subscription is registered for this device', async () => {
  sub = subscription(KEY);
  await push.syncPush();
  assert.deepEqual(server.registered, [sub.endpoint]);
  assert.equal(await push.pushState(), 'on');
});

test('a push service the server refuses shows as unavailable, not on', async () => {
  server.reject = true;
  push.resetPush();
  await push.syncPush();
  assert.equal(await push.pushState(), 'unavailable');
  await assert.rejects(push.enablePush(), /can't send notifications to this browser/);
  server.reject = false;
});

test('push turned off on the server shows as unavailable', async () => {
  server.enabled = false;
  push.resetPush();
  await push.syncPush();
  assert.equal(await push.pushState(), 'unavailable');
  sub = null;
  assert.equal(await push.pushState(), 'unavailable', 'also with no subscription here');
  server.enabled = true;
});

test('a subscription for another server key is replaced', async () => {
  sub = subscription(OTHER_KEY);
  server.registered = [];
  await push.syncPush();
  assert.deepEqual([...new Uint8Array(sub.options.applicationServerKey)], [...bytes(KEY)]);
  assert.equal(server.registered.length, 1);
});

test('nothing is registered without permission', async () => {
  Notification.permission = 'default';
  server.registered = [];
  await push.syncPush();
  assert.equal(server.registered.length, 0);
  assert.equal(await push.pushState(), 'default');
  Notification.permission = 'granted';
});
