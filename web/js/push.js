// Web Push: subscribe this device with the server's VAPID key.
import { get, post, del } from './api.js';

export const pushSupported = () =>
  'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;

export const standalone = () =>
  matchMedia('(display-mode: standalone)').matches || navigator.standalone === true;

export const isIOS = () => /iP(hone|ad|od)/.test(navigator.userAgent)
  || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);

/** 'unsupported' | 'needs_install' | 'denied' | 'default' | 'on' | 'off' */
export async function pushState() {
  if (!pushSupported()) return isIOS() && !standalone() ? 'needs_install' : 'unsupported';
  if (Notification.permission === 'denied') return 'denied';
  if (Notification.permission === 'default') return 'default';
  const sub = await currentSubscription();
  return sub ? 'on' : 'off';
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
  await post('/push/subscriptions', sub.toJSON());
}

export async function disablePush() {
  const sub = await currentSubscription();
  if (!sub) return;
  await del('/push/subscriptions/' + encodeURIComponent(sub.endpoint)).catch(() => {});
  await sub.unsubscribe();
}
