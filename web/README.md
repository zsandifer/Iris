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
├── cards.js          chat cards (result, connect, sign-in, API keys, approval, ...)
├── status.js         tool activity to plain-language phrases
├── feed.js           For you: story cards, posts, full-screen stories
├── lists.js          Ideas, Goals, Library
├── computer.js       Iris's computer: live view, chat peek, Watch / Take over
├── hub.js            Iris hub: Logins & wallet, files
├── connect.js        Connections: catalog, setup guides, sign-in, rules, trusted people
├── settings.js       Settings, AI engine, devices, people, activity log
├── push.js           Web Push subscription
├── md.js             small, safe Markdown renderer
├── photo.js          shared photo dialog, history, pinch and pan gestures
├── photo-math.js     pure photo zoom and pan geometry
├── ui.js             icons, rows, sheets, time helpers
├── viewport.js       visible viewport sizing and phone keyboard detection
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

The server names the service-worker cache `iris-shell-<version>-<bundle hash>`
from the Iris package version and the paths and bytes of every embedded asset
(including the worker template). Web changes invalidate it automatically;
identical bundles keep the same name. There is no manual cache version to bump.

Development: `IRIS_WEB_DIR=web iris serve ...` serves these files from disk,
so a browser reload picks up changes without rebuilding. The development worker
also hashes the files served from disk. For screenshots or
a quick look with content, seed a fresh data directory with fake demo data:

```sh
iris init --config $T/config.toml --data-dir $T/data
iris demo-seed --config $T/config.toml --data-dir $T/data --assets scripts/demo/images
```

The standalone iPhone keyboard regression uses an external Playwright
install with its WebKit browser; it adds no app dependencies:

```sh
NODE_PATH=/path/to/node_modules node scripts/e2e/keyboard.mjs /tmp/keyboard-frames.json
```

It checks synchronous composer pre-shrink on pointerdown/touchstart using
an orientation-specific remembered keyboard height, reconciliation when
the real resize arrives, and restoration on blur or the 700ms deadline.
It also resizes the viewport and injects document scrolling and visual viewport
pans, recording every animation frame. The header stays at zero in visible
viewport coordinates (its layout `getBoundingClientRect().top` minus
`visualViewport.offsetTop`), with stable height and avatar. Native iPhone
keyboard animation still needs a device check.

`scripts/e2e/photos.mjs` uses the same Playwright setup and an iPhone
descriptor to check photo surfaces, safe areas, focus, history, dismissals,
carousel taps, story pause, reduced motion, and zoom/pan bounds. It uses
native taps and injected touch lists for pinch gestures (Playwright's
touchscreen API provides only single-touch taps). Set `WEBKIT` to the
installed browser's `pw_run.sh` if its version differs from Playwright's.
