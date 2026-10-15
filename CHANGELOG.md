# Changelog

All notable changes to Iris are documented here, following
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - unreleased

### Added

- Chat times and a jump-to-latest button: day separators ("Today",
  "Yesterday", the weekday, or the date), a small time in the phone's own
  format under the last message of each run from the same sender (or after a
  pause of more than 10 minutes), and a round button above the input once
  scrolled up that glides back to the newest message and counts new messages
  that arrived meanwhile, without moving the conversation.

- Memory in the app: Iris hub → Memory with search, You / People / Projects /
  Routines, pinned notes first and recent changes marked; a note sheet to edit,
  see the chat it came from, read its history with Undo, pin it or forget it
  (with Undo); a "Noted: …" line under replies that saved something; the
  morning note card with Undo per change, "and N more" and a Memory link; and
  Settings → Memory (Pause learning, Morning note, Export as Markdown, Forget
  everything). Morning-note changes now say whether they were new, updated or
  done. `iris demo-seed` adds sample memory, and its Main chat history is
  visible again.

- Memory backend: bounded deterministic expiry and low-score archiving with
  undo, protected pinned/identity notes, confirmed owner-only Forget everything
  with stale-turn/Dream protection and non-reused IDs, and a complete Markdown
  download. Existing pause-learning and morning-note settings remain available.
  Reset keeps chats and backup copies. A five-category synthetic fake-CLI memory
  evaluation runs in `cargo test`, with an optional real-CLI smoke script.

- Nightly memory tidying through the owner's Background engine: bounded private
  chat input, strict validated decisions, history snapshots and undo, a guard
  against setting aside too many notes, and a success-only progress cursor.
  The backend supplies one morning-note card a day with per-change undo and a
  Memory link. Configure `[dream]` (3am,
  four new messages by default). Pause learning and morning-note preferences
  are owner settings; pausing learning also stops agent memory writes while
  recall and owner edits remain available. Generated Idea prompts stay out of
  Dream evidence, and historical chats cannot reverse newer corrections or
  resurrect forgotten facts through paraphrases. A Forget or text correction
  excludes all older unprocessed chat evidence; newer owner assertions remain
  learnable. Existing removal history initializes that source boundary on upgrade.

- Routines: jobs Iris runs on a schedule (once, daily / weekly / monthly /
  yearly rules, or cron), with time zones and daylight-saving handling,
  end dates and run limits, missed-run and retry rules, Markdown and text
  reference documents, and an approval step before buying or sending. Iris
  proposes routines in chat; they start only after your tap.

- Personal and shared chat with photos, streaming replies and saved history.
- Claude Code, Codex, Gemini and OpenCode engines, with installation and sign-in
  from the app, automatic model ranking and effort selection.
- Google, GitHub and custom connections through a gateway with approval cards
  and rules for trusted people; a guided Google setup wizard.
- One-tap connections through vendors' hosted MCP servers: Notion, Todoist,
  Airtable, monday.com, Miro, Canva, Cal.com, Granola, Fireflies, Zapier,
  Linear, Sentry, Cloudflare, Supabase, Netlify, Neon, Hugging Face,
  Readwise, and (asking before every action) Stripe and Mercury.
- Connections grouped by Productivity, Development, Money and Lifestyle, with
  Google listed as its services; each service has a permission screen with
  Allow / Ask / Off per action and an on/off switch, enforced in the gateway.
- Google adds Docs, Sheets, Slides, Forms, Tasks and Contacts; existing
  Google connections keep working and are asked to sign in again to add them.
- Iris’s computer: watch its browser, take over, and fill saved logins and
  cards there yourself. The browser is Microsoft’s Playwright MCP server,
  installed and updated from the app at a release pinned after the real
  browser test passes (CI bumps it weekly); Iris only wraps it (reviewed tools,
  live view, take over) instead of automating Chromium itself. Sign-ins are
  filled by Iris without the agent seeing them (`fill_saved_login`,
  `fill_saved_card` with element refs; cards ask you every time); the agent
  cannot read a page where a password was typed until it moves on (in any
  tab, or back in history), and cannot act on one where a saved password or
  card Iris filled is still on it (it can read it, with the secret
  hidden, to see whether a sign-in worked). The agent can restart a stuck
  browser (`restart_browser`, sign-ins kept) and post a card in the chat
  where you save a login for the open site (`ask_to_save_login`). The chat
  shows only the live picture of the browser, and keeps it for at least a
  minute after the turn (until you write again or take over). Sites that
  fight automation (like X): take over and sign in yourself.
- Iris’s own browser lives at one fixed path that `deploy/setup-engines.sh`
  approves for Chrome’s sandbox where AppArmor restricts user namespaces
  (Ubuntu 23.10+); when the sandbox cannot start, the app shows the exact
  command for that server.
- A personal For you feed, Ideas, Goals, Library and push notifications.
- Memory is kept as notes in Iris’s database, with a history of every change
  (so changes can be undone) and keyword search. The memory folder under
  System files is now a read-only Markdown copy (`MEMORY.md`, what the agent
  always keeps in mind, plus one file per topic) that Iris rewrites after
  every change. Existing memory files are imported once on upgrade, with the
  originals kept in `memory-backup/`; `iris import openclaw` imports memory
  the same way. The agent's `read_memory` / `write_memory` tools keep
  working on the new files.
- What the agent always keeps in mind is packed from the memory notes under a
  fixed size: pinned notes first, then notes about who you are, then the most
  important and recently used, whole notes only (instead of cutting the file
  off at 12,000 characters). Saving an ordinary note no longer restarts a
  Codex session, and a note that moved into `MEMORY.md` while a chat was
  answering is never removed by that chat's out-of-date copy.
  Ordinary notes fill spare capacity too, and resumed Codex chats receive
  current memory after another chat saves or corrects a note.
- Owner-private memory recall tools and past conversation keyword search, with
  secret/private-text filtering (including AWS secret-access-key assignments),
  duplicate protection, similarity hints,
  history-backed corrections and Forget/Undo. Owner-only Memory API supports
  search, topic filters, detail, editing and pinning; private turn writes persist
  Noted metadata and emit an owner-only event for the app.
- Optional separation of Iris and its agents into different OS users.
- Self-awareness of setup and connection status, with sign-in and setup cards.
- An installable mobile app with keyboard handling and a full-screen photo viewer.
- Version and build information in Settings and automatic offline cache updates.

### Fixed

- Security issues from #12: stronger approval, browser secret protection,
  private-data isolation and shared-chat history boundaries.
- Google setup guidance for required Branding fields and authorized domains.
