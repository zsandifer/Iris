// Web Push: subscribe this device with the server's VAPID key.
import { get, post, del } from './api.js';

export const pushSupported = () =>
  'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;

export const standalone = () =>
  matchMedia('(display-mode: standalone)').matches || navigator.standalone === true;

export const isIOS = () => /iP(hone|ad|od)/.test(navigator.userAgent)
  || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);

// Whether the server takes this browser's subscription for the signed-in
// device: null (not known yet), true, or false (push is off on the server,
// or it doesn't accept this browser's push service).
let accepted = null;
let syncing = null;

/**
 * 'unsupported' | 'needs_install' | 'denied' | 'default' | 'on' | 'off' |
 * 'unavailable' (subscribed here, but the server can't deliver to it)
 */
export async function pushState() {
  if (!pushSupported()) return isIOS() && !standalone() ? 'needs_install' : 'unsupported';
  if (Notification.permission === 'denied') return 'denied';
  if (Notification.permission === 'default') return 'default';
  await syncing;
  const sub = await currentSubscription();
  if (!sub) return 'off';
  return accepted === false ? 'unavailable' : 'on';
}

/**
 * After sign-in or pairing: the browser keeps its push subscription across
 * devices being revoked and re-paired, but the server only knows it for the
 * device that registered it. Register it for this device (an upsert by
 * endpoint) so what Settings shows matches what gets delivered.
 */
export function syncPush() {
  accepted = null;
  syncing = (async () => {
    if (!pushSupported() || Notification.permission !== 'granted') return;
    let sub = await currentSubscription();
    if (!sub) return;
    const { enabled, public_key: key } = await get('/push/vapid-public-key');
    if (!enabled || !key) { accepted = false; return; }
    // A subscription made for another server key can't get this server's pushes.
    const had = sub.options?.applicationServerKey;
    if (had && !sameBytes(new Uint8Array(had), keyBytes(key))) {
      await sub.unsubscribe().catch(() => {});
      const reg = await navigator.serviceWorker.ready;
      sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(key) });
    }
    await register(sub);
  })().catch(() => {});
  return syncing;
}

/** Forget what the server said (signed out). */
export function resetPush() {
  accepted = null;
  syncing = null;
}

const sameBytes = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);

async function register(sub) {
  try {
    await post('/push/subscriptions', sub.toJSON());
    accepted = true;
  } catch (e) {
    if (e.status !== 400) throw e;
    accepted = false;
    throw new Error("Your Iris server can't send notifications to this browser.");
  }
}

async function currentSubscription() {
  const reg = await navigator.serviceWorker.getRegistration();
  return reg ? reg.pushManager.getSubscription() : null;
}

function keyBytes(b64) {
  const s = atob(b64.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (b64.length % 4)) % 4));
  return Uint8Array.from(s, (c) => c.charCodeAt(0));
}

export async function enablePush() {
  if (!pushSupported()) {
    throw new Error(isIOS() ? 'Add Iris to your Home Screen first (Share, then Add to Home Screen).' : "This browser can't show notifications.");
  }
  const perm = await Notification.requestPermission();
  if (perm !== 'granted') throw new Error('Notifications are blocked. You can allow them in your phone settings.');
  const { enabled, public_key: key } = await get('/push/vapid-public-key');
  if (!enabled || !key) throw new Error('Notifications are turned off on the server.');
  const reg = await navigator.serviceWorker.ready;
  let sub = await reg.pushManager.getSubscription();
  if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(key) });
  await register(sub);
}

export async function disablePush() {
  const sub = await currentSubscription();
  if (!sub) return;
  await del('/push/subscriptions/' + encodeURIComponent(sub.endpoint)).catch(() => {});
  await sub.unsubscribe();
  accepted = null;
}
