# The Iris app

The phone app (installable PWA) that `iris serve` serves at `/`. The files
are embedded in the binary at build time (`src/server/web.rs`), so a release
binary needs nothing else. There is no build step and no Node.js toolchain:
the code is plain ES modules, with Preact and htm vendored in `vendor/`
(see `vendor/LICENSES.md`).

```
index.html            shell (CSP: scripts only from this origin)
app.css               all styles; tokens from docs/DESIGN.md
manifest.webmanifest  home-screen install
sw.js                 offline shell, push notifications, notification taps
icons/                placeholder icons
js/
├── app.js            shell: pairing, top bar, drawer, tabs, routing, boot
├── store.js          state, loaders, WebSocket event handling
├── api.js            REST client, authed image URLs, WebSocket
├── chat.js           chat, status phrases, receipts, composer (photos, voice input)
├── cards.js          chat cards (result, connect, sign-in, setup, approval, ...)
├── status.js         tool activity to plain-language phrases
├── feed.js           For you: story cards, posts, full-screen stories
├── lists.js          Ideas, Goals, Library
├── hub.js            Iris hub: Connections, Logins & wallet, computer, files
├── settings.js       Settings (voice, people), AI engine, devices, activity log
├── voice.js          voice notes (server voice or this device), recording + transcription
├── push.js           Web Push subscription
├── md.js             small, safe Markdown renderer
├── ui.js             icons, rows, sheets, time helpers
└── h.js              Preact + htm exports
```

Rules:

- Talk only to the API in `docs/API.md`. Binary routes need the token, so
  images go through `Img` / `blobUrl` (object URLs).
- Follow `docs/DESIGN.md`: nothing technical by default. Model names,
  efforts, and tokens appear only in Settings, AI engine.
- Every file here must be listed in `ASSETS` in `src/server/web.rs`, and
  every JS module in `SHELL` in `sw.js` (a test checks both). Imports must
  be relative (`./x.js`).

Development: `IRIS_WEB_DIR=web iris serve ...` serves these files from disk,
so a browser reload picks up changes without rebuilding. For screenshots or
a quick look with content, seed a fresh data directory with fake demo data:

```sh
iris init --config $T/config.toml --data-dir $T/data
iris demo-seed --config $T/config.toml --data-dir $T/data --assets scripts/demo/images
```
