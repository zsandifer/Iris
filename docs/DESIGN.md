# Iris app design

The product and UX spec for the Iris phone app (installable PWA). It was
decided over several mock-up rounds. The goal: **easy enough that anyone's mom
could use it, and transparent enough that a tinkerer can find and change
everything.** Iris takes inspiration from consumer assistant apps but has its
own personality. It is not a clone.

## Principles

1. **Nothing technical by default.** No model names, effort levels, tokens or
   error banners. Problems are explained by Iris in plain words, with a card
   to fix them.
2. **Honest and transparent.** Iris always shows that it is working, says when
   it is stuck, and explains why it showed you something. Every action can be
   inspected if you dig.
3. **Never touch the server again.** After pairing a phone, everything is done
   from the app: sign-ins, connectors, engines, models and updates.
4. **Model agnostic.** Works with any installed harness (Claude Code, Codex,
   Gemini CLI, OpenCode with local models), in any combination.
5. **You own everything.** Self-hosted; plain-text persona and memory files;
   no telemetry.

## Look

- Theme follows the phone. Light is warm cream (`#fbf8f3`) and dark is deep
  plum (`#120f18`). The accent is violet (`#7c5cf6` light, `#a78bfa` dark).
  User bubbles use a violet→orchid gradient.
- Soft cards (radius ~20px), labeled bottom tabs, serif page titles
  (`ui-serif`), and system UI font for body text.
- Avatar: a character, still to be designed. Use a placeholder until then;
  the user can upload a custom image.

## Navigation

- **Top bar:** ☰ on the **left** opens the drawer, followed by Iris's avatar,
  name and status line.
- **Bottom tabs:** Chat · For you · Ideas · Goals · Library.
- **Drawer (slides from the left):** search, then chats (Main chat, side
  chats, + New side chat), then "Together" (shared chats). An **Iris profile**
  entry is pinned at the bottom and opens the **Iris hub**.
- **Iris hub:** Connections · Logins & wallet · Iris's computer · System files
  · Settings.

## Chat

- One persistent **Main chat**, plus side chats (from the drawer, from Ideas,
  and from "Talk about it" on feed posts).
- Input: ＋ (photos) · text · 🎙 (voice input).
- **While Iris works:** a short, specific shimmering status phrase such as
  "Checking your calendar…" or "Reading your email…". There are no "…" dots
  and no quick-reply chips.
- **Browser peek:** only while Iris is browsing or researching, a small live
  thumbnail of its browser appears. Tap it to watch full screen or take over.
- **Approval card:** rendered by Iris (not the agent) for sending email or
  messages, purchases, deleting, bulk exports, and anything that reaches a
  new recipient through a connected service. It says why it asks ("Sends a
  message to someone new: sam@example.com"), shows the real preview (to,
  subject, the exact text, other fields) built from the held call's
  arguments, a "Show exactly what will be sent" toggle with the raw
  request, a countdown, and Deny / Send (Pay, Delete, Allow). It sits just
  above the reply of the turn that asked, and the status line says
  "Waiting for your OK…" meanwhile. Afterwards it reads "You approved
  this", "You said no, so I didn't do it", "No answer in time, so I didn't
  do it", or "Cancelled. Nothing was done." Messages only to trusted people
  go out without a card.
- **Rich result cards:** for example a restaurant with an image, details and
  one action.
- **Long tasks:** Iris says "I'll ping you when it's done", then sends a push.
- **Stuck:** if a turn dies, Iris sends a push saying "Iris got stuck" with a
  retry, so the user is never left waiting.
- **Voice replies:** off by default, toggled in Settings. Replies arrive as
  voice notes plus text.
- **Push notifications:** every message from Iris, unless that chat is open.
  Quiet hours are available.
- **Reconnect cards:** when an engine's sign-in expires or a connector drops,
  Iris posts a "Reconnect Claude/Google" card and sends a push. The card runs
  the official sign-in flow (link or device code, paste code) and moves
  through waiting → connected ✓ / failed, retry. While it is down, Iris uses
  the next-ranked model and says so once.
- **Setup by chat:** "help me set up Google" returns a Connect card that
  opens the step-by-step setup (and shows Connected once done). Installing
  a new engine works the same way: install → sign-in card → done. Engines
  that sign in with an API key (Gemini, OpenCode providers) get a key field
  right in the card, checked on save.
- **Paused:** while Iris is paused, the status line says "paused" and a bar
  at the top of the chat offers Resume; messages get a short "I'm paused"
  reply.

## For you (feed)

- An Instagram-like, curated feed.
- **Top row:** tall story cards (photo + topic name), scrolling sideways, one
  per topic. Tapping one opens **full-screen stories**, 1–5 per topic. Tap to
  go to the next story, swipe left/right to change topic, swipe up to reply to
  Iris (opens a chat about it), swipe down to close.
- **Feed:** posts with the source's name and favicon, a swipeable stack of
  the page's **original photos** (never generated), a short summary, a "why
  you'd like this" line, and ♡ · 💬 Talk about it · Visit ›.
- Steered by likes and conversations (no prompt box).
- Size adapts: about 10 posts a day, growing to about 30 if the user keeps
  reaching the end, shrinking if rarely opened, and auto-pausing after 2
  weeks unused (Iris mentions it once).
- Push only for big, personal items (price drops, events).
- Pipeline is deterministic code: RSS/APIs (YouTube via Google, Reddit,
  Bluesky, Mastodon, creator RSS) → OpenGraph extraction → one batched
  cheap-model summary call per day → rule-based ranking.

## Ideas, Goals, Library

- **Ideas:** things Iris could do for you. "Do it" opens a side chat and
  starts; "Not now" dismisses.
- **Goals:** Iris suggests and you confirm. Each goal shows status text that
  Iris keeps current, plus a checkbox.
- **Library:** what Iris made (reports, images), photos you sent, and saved
  (♡) feed posts.

## Iris hub

- **Connections:** connected services with plain-language rules ("Reads
  freely · asks before sending, buying, or deleting"), a small catalog to
  connect (Google, GitHub, Custom MCP server) with step-by-step guides and
  sign-in on the service's own page, and Trusted people. Each connection
  has an On switch, its rules (four plain choices), its actions (each can
  be set to run, ask, or never), recent use, and advanced settings. A
  connection a tripwire paused shows why, with Resume.
- **Logins & wallet:** an encrypted store. The app explains that Iris can use
  these but never sees them. Cards always ask before paying.
- **Iris's computer:** a live screen with Watch / Take over, recent activity,
  Pause Iris (stops everything now and starts nothing new until resumed),
  Trusted people, and Disconnect everything (turns every connection off).
- **System files:** the persona markdown files and memory/workspace folders,
  each with a plain-language label, editable.
- **Settings:**
  - Name, picture, voice replies, notifications, quiet hours.
  - Advanced: **AI engine**, devices, people, activity log (all steps, or
    just connected services with whether each ran on its own or you
    approved it).
- **AI engine:** one selector per slot (Chat, Hard requests, Background,
  Feed), showing only models actually installed. Each slot defaults to
  "Automatic (recommended: X)" from the built-in ranking, and can be pinned.
  The screen also lists the engines on this server with their sign-in state
  and Set up / Reconnect / Add key (an API key instead of a subscription,
  stored encrypted).

## People

- Each person (for example a partner) gets **their own Iris** with separate
  memory, plus shared "Together" chats.
- Pairing: one-time code → long-lived per-device key, revocable per device.
