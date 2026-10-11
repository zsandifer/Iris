# Contributing to Iris

Thanks for wanting to help! Iris is a self-hosted personal AI agent app that
you own. Contributions of every size are welcome: bug reports, docs, design,
harness adapters, connectors and features.

## Before you start

- **Read [AGENTS.md](AGENTS.md).** It lists the invariants every change must
  keep (Iris wraps official CLIs and never becomes a harness, never touches
  subscription credentials, keeps personal data out of the repo, stays small).
- **Read [docs/DESIGN.md](docs/DESIGN.md)** for anything user-facing. The bar
  is "easy enough for anyone's mom, transparent enough for a tinkerer".
- **Small fixes** (typos, obvious bugs, docs): just open a pull request.
- **Features, new adapters, new dependencies or UX changes:** open an issue
  first and describe the problem and your proposal. This saves you from
  building something that can't be merged.

## How decisions are made

Iris is maintained by its creator, who has final say on scope, design and
what gets merged. The project favors simplicity, privacy and "it just works
from the app" over configurability. A good idea may still be declined if it
adds complexity most users don't need; when that happens, we'll explain why.

## Pull requests

1. Fork the repo and branch from `main`.
2. Keep changes focused: one topic per PR, with small, descriptive commits.
3. Make sure these pass:
   ```sh
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. Add tests for new behavior. For harness adapters, include synthetic
   samples modeled on real CLI output, never captured output with real paths
   or account data.
5. Update `docs/API.md` if you change routes or events, and `docs/DESIGN.md`
   if you change UX.
6. For UI changes, add before/after screenshots to the PR.
7. Sign off your commits (see below).

AI-assisted contributions are welcome. You are responsible for every line you
submit: review it, test it, and be able to explain it.

## Sign-off (DCO)

Every commit must be signed off under the
[Developer Certificate of Origin](https://developercertificate.org/). This
certifies that you wrote the change, or have the right to submit it under the
project's licenses:

```sh
git commit -s -m "Add OpenCode adapter"
```

This adds a `Signed-off-by: Your Name <you@example.com>` line. By
contributing, you agree that your contributions are licensed under the
project's dual MIT / Apache-2.0 license.

## Security

**Do not open public issues for security problems.** Use GitHub's private
vulnerability reporting ("Report a vulnerability" on the Security tab). We
will acknowledge reports and work with you on a fix and disclosure timeline.

## Privacy

Never include real personal data (names, emails, chats, tokens, hostnames)
in issues, logs, tests or screenshots. Use fake data.

## Code of conduct

Be kind, be constructive, and assume good intent. Harassment or abuse of any
kind is not tolerated and will get you removed from the project.
