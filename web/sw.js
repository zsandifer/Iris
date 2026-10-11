// Iris service worker: offline app shell, push notifications, and opening
// the right chat when a notification is tapped. API calls are never cached.
const CACHE = 'iris-shell-v1';
const SHELL = [
  '/',
  '/app.css',
  '/manifest.webmanifest',
  '/icons/icon.svg',
  '/icons/icon-192.png',
  '/js/api.js',
  '/js/app.js',
  '/js/cards.js',
  '/js/chat.js',
  '/js/connect.js',
  '/js/feed.js',
  '/js/h.js',
  '/js/hub.js',
  '/js/lists.js',
  '/js/md.js',
  '/js/push.js',
  '/js/settings.js',
  '/js/status.js',
  '/js/store.js',
  '/js/ui.js',
  '/vendor/hooks.js',
  '/vendor/htm.js',
  '/vendor/preact.js',
];

self.addEventListener('install', (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (e) => {
  e.waitUntil(caches.keys()
    .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
    .then(() => self.clients.claim()));
});

// Network first (so updates show up at once), cache when offline.
self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET' || url.origin !== self.location.origin || url.pathname.startsWith('/api/')) return;
  e.respondWith(fetch(e.request).then((res) => {
    if (res.ok) {
      const copy = res.clone();
      caches.open(CACHE).then((c) => c.put(e.request, copy));
    }
    return res;
  }).catch(() => caches.match(e.request, { ignoreSearch: true })
    .then((hit) => hit || (e.request.mode === 'navigate' ? caches.match('/') : Response.error()))));
});

self.addEventListener('push', (e) => {
  let data = {};
  try { data = e.data ? e.data.json() : {}; } catch { data = { body: e.data && e.data.text() }; }
  const title = data.title || 'Iris';
  const options = {
    body: data.body || '',
    icon: '/icons/icon-192.png',
    badge: '/icons/icon-192.png',
    tag: data.conversation_id || data.kind || 'iris',
    renotify: true,
    data: { conversation_id: data.conversation_id || null, kind: data.kind || null },
  };
  e.waitUntil(self.registration.showNotification(title, options));
});

self.addEventListener('notificationclick', (e) => {
  e.notification.close();
  const cid = e.notification.data && e.notification.data.conversation_id;
  const url = cid ? `/#/chat/${cid}` : '/';
  e.waitUntil(self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((wins) => {
    for (const w of wins) {
      if (new URL(w.url).origin === self.location.origin) {
        w.postMessage({ type: 'open', url });
        return w.focus();
      }
    }
    return self.clients.openWindow(url);
  }));
});
