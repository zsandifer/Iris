# Changelog

All notable changes to Iris are documented here, following
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - unreleased

### Added

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
- Iris’s computer: watch its browser, take over, and fill saved logins securely.
- Iris’s own browser lives at one fixed path that `deploy/setup-engines.sh`
  approves for Chrome’s sandbox where AppArmor restricts user namespaces
  (Ubuntu 23.10+); when the sandbox cannot start, the app shows the exact
  command for that server.
- A personal For you feed, Ideas, Goals, Library and push notifications.
- Optional separation of Iris and its agents into different OS users.
- Self-awareness of setup and connection status, with sign-in and setup cards.
- An installable mobile app with keyboard handling and a full-screen photo viewer.
- Version and build information in Settings and automatic offline cache updates.

### Fixed

- Security issues from #12: stronger approval, browser secret protection,
  private-data isolation and shared-chat history boundaries.
- Google setup guidance for required Branding fields and authorized domains.
