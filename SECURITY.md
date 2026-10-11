# Security policy

Iris runs on your own machine and holds sensitive things: your chats, memory,
connector credentials and saved logins. We take security reports seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Report it privately with GitHub's
[Report a vulnerability](https://github.com/zsandifer/Iris/security/advisories/new)
button on the Security tab. Include what you found, how to reproduce it, and
the impact you expect.

You can expect an acknowledgement within a few days. We'll keep you updated
while we work on a fix, agree on a disclosure date with you, and credit you
in the advisory unless you'd rather stay anonymous.

## Supported versions

Iris is early and has no stable releases yet. Security fixes land on `main`.

## Scope

In scope: the `iris` binary, the web app it serves, the deploy scripts, and
how Iris drives the official CLIs (secret handling, the approval gate,
pairing and device auth, isolation between people).

Out of scope: vulnerabilities in the third-party CLIs themselves (Claude
Code, Codex, Gemini CLI, OpenCode) or in MCP servers you connect. Report
those to their vendors.
