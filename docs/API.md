# Iris API reference

This document covers the HTTP API, the WebSocket event stream, the turn
lifecycle, the data model, and the `iris mcp` bridge exposed by the Iris
daemon (`iris serve`). Everything here is derived from the source in `src/`;
if the two ever disagree, the code wins and this file has a bug.

## Contents

1. [Overview](#1-overview)
2. [Authentication and pairing](#2-authentication-and-pairing)
3. [REST routes](#3-rest-routes)
   - [3.1 Route index](#31-route-index)
   - [3.2 Health, pairing, me, devices, users](#32-health-pairing-me-devices-users)
   - [3.3 Conversations and messages](#33-conversations-and-messages)
   - [3.4 Attachments](#34-attachments)
   - [3.5 Feed](#35-feed)
   - [3.6 Ideas](#36-ideas)
   - [3.7 Goals](#37-goals)
   - [3.7a Activity and approvals](#37a-activity-and-approvals)
   - [3.8 Library](#38-library)
   - [3.9 System files and folders](#39-system-files-and-folders)
   - [3.10 Settings and avatar](#310-settings-and-avatar)
   - [3.11 Harnesses, sign-in, models, and slots](#311-harnesses-sign-in-models-and-slots)
   - [3.12 Connectors](#312-connectors)
   - [3.13 Credentials](#313-credentials)
   - [3.14 Push](#314-push)
   - [3.15 Jobs](#315-jobs)
   - [3.16 Voice](#316-voice)
   - [3.17 Engines (install and update)](#317-engines-install-and-update)
   - [3.18 Safety: pause, disconnect, trusted people](#318-safety-pause-disconnect-trusted-people)
   - [3.19 Iris's computer (browser)](#319-iriss-computer-browser)
4. [WebSocket: `GET /api/ws`](#4-websocket-get-apiws)
5. [Turn lifecycle](#5-turn-lifecycle)
6. [Data model](#6-data-model)
7. [Engine selection](#7-engine-selection)
8. [The iris-mcp bridge](#8-the-iris-mcp-bridge)
   - [The browser tools](#the-browser-tools)
9. [Push notifications](#9-push-notifications)
10. [Placeholders and TODO](#10-placeholders-and-todo)
11. [The app](#11-the-app)

---

## 1. Overview

### Base URL

The server listens on `bind` from `config.toml` (default `127.0.0.1:7420`).
All API routes live under `/api`. Examples in this document use a private
tailnet URL:

```
https://iris.example.ts.net/api/...
```

`GET /` serves the Iris phone app, an installable PWA embedded in the
binary (`web/`; see [section 11](#11-the-app)). `GET /console` serves a bare
HTML test console for debugging.

### Conventions

| Topic | Convention |
|---|---|
| Content type | Request and response bodies are JSON (`Content-Type: application/json`), except the raw-image upload routes and the binary download routes, which are called out. |
| Timestamps | Every `*_at` field and the event `at` field are **Unix epoch milliseconds** (integers). |
| IDs | Server-generated ids are **32 lowercase hex characters** (16 random bytes), e.g. `"9f2c4e1a7b3d5f60a1c2e3d4f5a6b7c8"`. |
| Nulls | Optional fields are present with value `null` rather than omitted (exceptions are noted, e.g. card fields). |
| Text limits | Length limits count Unicode characters (not bytes) unless stated otherwise. Values are usually trimmed before storage. |

### Error envelope

Errors produced by Iris use this shape:

```json
{"error": {"code": "not_found", "message": "conversation not found"}}
```

| Code | HTTP status | When |
|---|---|---|
| `bad_request` | 400 | Validation failure, malformed or missing JSON body, wrong `Content-Type` on a JSON route. The message is human-readable. |
| `unauthorized` | 401 | Missing, malformed, unknown, or revoked device token. Message: `missing or invalid device token`. |
| `invalid_code` | 401 | `POST /api/pair` with a wrong, used, or expired code. |
| `forbidden` | 403 | Owner-only route called by a non-owner (`only the owner can do this`), or a creator-only group action by another member (`only the person who created this group can do that`). |
| `not_found` | 404 | Unknown resource (message is `<thing> not found`), or an unknown route (`route not found`). |
| `conflict` | 409 | Creating a connector whose name already exists; deciding an approval card that is no longer pending. |
| `needs_client` | 409 | `POST /api/connectors/{id}/oauth/start` for a service without dynamic registration and no saved client. |
| `paused` | 409 | Running a job while Iris is paused. |
| `too_large` | 413 | Uploaded image larger than `max_upload_bytes`. |
| `unsupported_media_type` | 415 | Uploaded bytes are not PNG, JPEG, GIF, or WebP. |
| `locked` | 429 | Pairing is locked after repeated failures (see [section 2](#2-authentication-and-pairing)). |
| `internal` | 500 | Unexpected server error. Message is always `internal server error`; details only go to the server log. |
| `not_implemented` | 501 | `POST /api/voice/speak`. |

A few responses come from the HTTP framework (axum) and do **not** use the
envelope:

- Path or query parameters that fail to parse (for example `?limit=abc` or
  `?archived=yes`; booleans must be `true`/`false`): `400` with a plain-text
  body.
- A known path with the wrong method: `405` with an empty body.
- A raw-body upload (`POST /api/attachments`, `PUT /api/settings/avatar`)
  larger than the body limit below: `413` with a plain-text body. (On JSON
  routes an oversized body is reported as `400 bad_request` instead.)

### Body size limit

Every `/api` route has a request body limit of
**`max_upload_bytes + 64 KiB`** (`max_upload_bytes` defaults to 10 MiB). Image
uploads are additionally checked against `max_upload_bytes` itself
(`413 too_large`).

### Authentication requirements

| Route | Auth |
|---|---|
| `GET /` and the app's static files, `GET /console` (test console) | none |
| `GET /api/health` | none |
| `POST /api/pair` | none (rate limited) |
| `GET /api/oauth/callback` | none: a browser navigation from a service's sign-in page, tied to a sign-in the owner started by a one-time, 15-minute `state` (see [3.12](#312-connectors)) |
| everything else under `/api` | `Authorization: Bearer iris_<64 hex chars>` |

The scheme name `Bearer` is case-insensitive. The WebSocket can also
authenticate with a first frame instead of the header (see
[section 4](#4-websocket-get-apiws)).

**Owner-only** routes return `403 forbidden` for other users. They are marked
**Owner** in the route tables. There is exactly one owner (the first user,
created automatically when the database is initialized); additional people
are created by the owner for group conversations. Some group-conversation
actions are limited to the group's creator instead (see
[section 3.3](#33-conversations-and-messages)).

Binary routes (`/api/attachments/{id}`, `/api/library/{id}/content`,
`/api/avatar`) also require the header, so browser clients should `fetch()`
them and use an object URL rather than pointing an `<img src>` at them.

---

## 2. Authentication and pairing

Iris has no passwords. Each device gets a long-lived bearer token by
exchanging a short one-time **pairing code**.

### Getting a code

- On the server: `iris pair` creates a code for the owner. `iris pair --user
  <USER_ID>` creates one for another user. It prints the code and the URL to
  open (`public_url` from config, or the running server’s actual bound address,
  including a `--bind` override; config’s bind address when no saved listener exists).
- Through the API: the owner can call
  [`POST /api/users/{id}/pairing-code`](#post-apiusersidpairing-code) for any
  user, including additional people created with `POST /api/users`.

### Code format

| Property | Value |
|---|---|
| Length | 8 characters, displayed as `XXXX-XXXX` (e.g. `7KQM-3XPA`) |
| Alphabet | `23456789ABCDEFGHJKMNPQRSTVWXYZ` (no `0`, `1`, `I`, `L`, `O`, `U`) |
| Matching | Case-insensitive; every non-alphanumeric character (dashes, spaces) is ignored, so `7kqm 3xpa` matches `7KQM-3XPA` |
| Lifetime | 10 minutes |
| Use | One time; consumed atomically on success |
| Storage | Only `SHA-256("iris-pair:" + normalized code)` is stored |

### Lockout

Failed attempts are counted per listener peer IP in memory (forwarded
headers are not trusted). Five failed or concurrent attempts lock that
source for 15 minutes. Other sources can still pair. When a peer address
is unavailable, or the bounded source table is full, a fallback global
limit allows 500 attempts in the same window. Behind a reverse proxy,
clients sharing its peer address share its limit.

A successful pairing clears that source's failures. Creating a new code
with `iris pair` (or the owner API) clears existing lockouts on the next
attempt, without restarting the daemon. Outstanding unused codes remain
valid until used or expired. Restarting the daemon clears in-memory limits.

Revoking a device immediately cancels all of its event sockets and hands
back browser control if that device held it. Every queued browser input
also revalidates the device before acting.

### Tokens

| Property | Value |
|---|---|
| Format | `iris_` followed by 64 lowercase hex characters (32 random bytes) |
| Storage | Only `SHA-256("iris-token:" + token)` is stored; a database copy cannot be used to impersonate a device |
| Expiry | None; tokens are valid until the device is revoked |
| Rejected outright | Tokens without the `iris_` prefix or longer than 200 characters |

A device's `last_seen_at` is updated on authenticated requests, at most once
per minute.

### Revocation

`DELETE /api/devices/{id}` revokes one of the caller's own devices. The token
stops working immediately for HTTP, the device's push subscriptions are
deleted, and any open WebSocket for that device is closed within about a
minute (see [section 4](#4-websocket-get-apiws)).

---

## 3. REST routes

### 3.1 Route index

All paths are under `/api`. "Any" means any authenticated user.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/health` | none | Liveness and version |
| POST | `/pair` | none | Exchange a pairing code for a token |
| GET | `/me` | Any | Current user, device, Main chat id, quiet hours |
| PUT | `/me` | Any | Rename yourself |
| PUT | `/me/quiet-hours` | Any | Set or clear your notification quiet hours |
| GET | `/devices` | Any | Your devices |
| DELETE | `/devices/{id}` | Any | Revoke one of your devices |
| GET | `/users` | **Owner** | List all users |
| POST | `/users` | **Owner** | Create another person |
| POST | `/users/{id}/pairing-code` | **Owner** | Pairing code for a user |
| GET | `/conversations` | Any | List your conversations |
| POST | `/conversations` | Any (group: **Owner**) | Create a side or group conversation |
| GET | `/conversations/main` | Any | Your Main chat |
| GET | `/conversations/{id}` | Member | One conversation |
| PATCH | `/conversations/{id}` | Member (group: creator) | Rename or archive |
| DELETE | `/conversations/{id}` | Member (group: creator) | Delete (not the Main chat) |
| GET | `/conversations/{id}/members` | Member | List members |
| POST | `/conversations/{id}/members` | Group creator | Add a member |
| DELETE | `/conversations/{id}/members/{user_id}` | Member (others: group creator) | Leave, or remove a member |
| GET | `/conversations/{id}/messages` | Member | Page through messages |
| POST | `/conversations/{id}/messages` | Member | Send a message, start a turn (`202`) |
| POST | `/conversations/{id}/cancel` | Member | Cancel the running turn |
| POST | `/conversations/{id}/read` | Member | Mark read |
| GET | `/attachments` | Any | Photos you sent in chats |
| POST | `/attachments` | Any | Upload an image (raw body) |
| GET | `/attachments/{id}` | Uploader or member | Download an image |
| POST | `/messages/{id}/decision` | Approver | Approve or deny an approval card |
| GET | `/activity` | Any | What Iris did with tools, newest first |
| GET | `/feed` | Any | List feed posts (`?liked=true`: saved posts) and the feed status |
| GET | `/feed/status` | Any | The feed status alone |
| POST | `/feed/activity` | Any | Report that the feed was opened or read to the end |
| POST | `/feed/pause` | Any | Pause the daily posts |
| POST | `/feed/resume` | Any | Resume the daily posts |
| GET | `/feed/sources` | Any | List your feed sources |
| POST | `/feed/sources` | Any | Add a source (link, `r/name`, `@handle`, ...) |
| PATCH | `/feed/sources/{id}` | Any | Turn a source on or off |
| DELETE | `/feed/sources/{id}` | Any | Remove a source |
| GET | `/feed/images/{file}` | Any | A cached feed photo or icon |
| POST | `/feed/refresh` | **Owner** | Run the feed pipeline now (`202`) |
| GET | `/feed/prompt` | Any | Read the feed prompt |
| PUT | `/feed/prompt` | **Owner** | Set the feed prompt |
| DELETE | `/feed/{id}` | Any | Delete a post |
| POST | `/feed/{id}/like` | Any | Like or unlike |
| POST | `/feed/{id}/discuss` | Any | Open a side chat about a post |
| GET | `/ideas` | Any | List ideas |
| PATCH | `/ideas/{id}` | Any | Change an idea's status |
| DELETE | `/ideas/{id}` | Any | Delete an idea |
| POST | `/ideas/{id}/start` | Any | Start an idea in a new chat (`202`) |
| GET | `/goals` | Any | List goals |
| POST | `/goals` | Any | Create a goal |
| PATCH | `/goals/{id}` | Any | Update a goal |
| DELETE | `/goals/{id}` | Any | Delete a goal |
| GET | `/library` | Any | List library items |
| GET | `/library/{id}` | Any | One library item |
| DELETE | `/library/{id}` | Any | Delete a library item |
| GET | `/library/{id}/content` | Any | Download item content |
| GET | `/files` | **Owner** | System files and folders |
| GET | `/files/system/{name}` | **Owner** | Read a system file |
| PUT | `/files/system/{name}` | **Owner** | Write a system file |
| GET | `/files/browse/{area}` | **Owner** | List a folder |
| GET | `/files/content/{area}` | **Owner** | Read a text file |
| PUT | `/files/content/{area}` | **Owner** | Write a text file |
| DELETE | `/files/content/{area}` | **Owner** | Delete a file or empty folder |
| GET | `/settings` | Any | Read settings |
| PUT | `/settings` | **Owner** | Update settings |
| PUT | `/settings/avatar` | **Owner** | Upload agent avatar (raw body) |
| DELETE | `/settings/avatar` | **Owner** | Reset agent avatar |
| GET | `/avatar` | Any | Agent avatar image |
| GET | `/harnesses` | Any | CLI harness status |
| POST | `/harnesses/refresh` | **Owner** | Re-probe harnesses |
| GET | `/harnesses/{name}/login` | **Owner** | Sign-in session state |
| POST | `/harnesses/{name}/login` | **Owner** | Start the CLI's own sign-in |
| DELETE | `/harnesses/{name}/login` | **Owner** | Cancel a sign-in |
| POST | `/harnesses/{name}/login/code` | **Owner** | Pass the pasted code to the CLI |
| GET | `/models` | Any | Model catalog, availability, and slots |
| GET | `/models/slots` | Any | Slots with their ranked available models |
| PUT | `/models/slots/{slot}` | **Owner** | Pin a slot, or return it to automatic |
| GET | `/engines` | **Owner** | Installable engines and their state |
| POST | `/engines/{name}/install` | **Owner** | Install (or update) an engine |
| POST | `/engines/{name}/update` | **Owner** | Update an engine Iris installed |
| DELETE | `/engines/{name}` | **Owner** | Remove an engine Iris installed |
| GET | `/connectors` | **Owner** | List connectors |
| POST | `/connectors` | **Owner** | Create a connector |
| PATCH | `/connectors/{id}` | **Owner** | Update a connector |
| DELETE | `/connectors/{id}` | **Owner** | Delete a connector |
| POST | `/connectors/{id}/connect` | **Owner** | Enable |
| POST | `/connectors/{id}/disconnect` | **Owner** | Disable |
| POST | `/connectors/{id}/resume` | **Owner** | Turn on and clear a tripwire pause |
| PUT | `/connectors/{id}/policy` | **Owner** | Approval rules (per action, Google services on/off) |
| GET | `/connectors/{id}/tools` | **Owner** | Its tools and what the rules do with each |
| GET | `/connectors/{id}/permissions` | **Owner** | A service's permission screen: actions with Allow / Ask / Off |
| POST | `/connectors/{id}/oauth/start` | **Owner** | Start signing it in (OAuth) |
| POST | `/connectors/{id}/oauth/sign-out` | **Owner** | Sign it out (keeps its client and rules) |
| POST | `/connectors/{id}/oauth/client` | **Owner** | Save the owner's own OAuth client |
| GET | `/connectors/catalog` | **Owner** | Services to connect, by group (Google as its services, GitHub, one-tap services, custom) |
| POST | `/connectors/catalog/{service}` | **Owner** | Set up a catalog service |
| GET | `/connectors/google/setup` | **Owner** | Saved Google wizard progress, Console links and exact callback |
| PUT | `/connectors/google/setup` | **Owner** | Save a wizard step or return to sign-in |
| POST | `/connectors/google/setup/client` | **Owner** | Validate a single paste; optionally save the Google client |
| POST | `/connectors/google/setup/error` | **Owner** | Report a Google error shown before returning to Iris |
| GET | `/connectors/activity` | **Owner** | Connector calls, newest first |
| POST | `/oauth/complete` | **Owner** | Finish a sign-in with the address the browser landed on |
| GET | `/oauth/callback` | none | Where services send the browser after sign-in |
| GET | `/safety` | **Owner** | Pause state, trusted people, limits |
| PUT | `/safety` | **Owner** | Pause or resume Iris; set trusted people |
| POST | `/safety/disconnect-all` | **Owner** | Turn every connector off |
| GET | `/harnesses/{name}/keys` | **Owner** | Saved API keys (names only) |
| PUT | `/harnesses/{name}/keys/{env}` | **Owner** | Save an engine API key (checked right away) |
| DELETE | `/harnesses/{name}/keys/{env}` | **Owner** | Remove an engine API key |
| GET | `/credentials` | **Owner** | List credential metadata |
| POST | `/credentials` | **Owner** | Create a credential |
| PATCH | `/credentials/{id}` | **Owner** | Update a credential |
| DELETE | `/credentials/{id}` | **Owner** | Delete a credential |
| GET | `/browser` | **Owner** | Iris's browser: status and setup state |
| POST | `/browser/start` | **Owner** | Open the browser |
| POST | `/browser/stop` | **Owner** | Close the browser (sign-ins are kept) |
| POST | `/browser/takeover` | **Owner** | Take control from this device |
| POST | `/browser/handback` | **Owner** | Give control back to Iris |
| POST | `/browser/navigate` | **Owner** | Open a page while in control |
| POST | `/browser/install` | **Owner** | Install Chromium for Iris (`engines`) |
| POST | `/browser/sandbox` | **Owner** | Allow (or stop allowing) running without Chrome's sandbox where it cannot start |
| GET | `/push/vapid-public-key` | Any | VAPID public key |
| GET | `/push/subscriptions` | Any | Your push subscriptions |
| POST | `/push/subscriptions` | Any | Subscribe this device |
| DELETE | `/push/subscriptions/{id}` | Any | Unsubscribe |
| POST | `/push/test` | Any | Send yourself a test push |
| GET | `/jobs` | **Owner** | Scheduled job status |
| POST | `/jobs/{name}/run` | **Owner** | Run a job now (`202`) |
| POST | `/voice/speak` | Any | Placeholder, always `501` |
| GET | `/ws` | token | WebSocket event stream |

"Member" routes return `404 not_found` (`conversation not found`) when the
caller is not a member of the conversation, and for hidden job conversations.

Feed, idea, goal, and library routes only ever see the caller's own items;
another user's id behaves like an unknown id (`404`).

### 3.2 Health, pairing, me, devices, users

#### `GET /api/health`

No auth.

```json
{"ok": true, "name": "iris", "version": "0.1.0", "build_id": "a1b2c3d4"}
```

`version` is `CARGO_PKG_VERSION`; `build_id` is the first eight characters of
the Git commit captured at build time, or `"dev"` when Git metadata is unavailable.
Settings shows these together at the bottom. No timestamp or dirty-tree marker
is included.

#### `POST /api/pair`

No auth. Exchanges a pairing code for a device token.

| Field | Type | Notes |
|---|---|---|
| `code` | string | Required. Case and separators ignored. |
| `device_name` | string | Optional. Trimmed and cut to 80 characters; defaults to `"Device"`. |

```json
{"code": "7KQM-3XPA", "device_name": "Phone"}
```

Response `200`:

```json
{
  "token": "iris_<64 hex chars>",
  "device": {"id": "...", "user_id": "...", "name": "Phone",
             "created_at": 1760000000000, "last_seen_at": null, "revoked_at": null}
}
```

The token is shown only once. Errors: `401 invalid_code`, `429 locked`.

#### `GET /api/me`

```json
{"user": {User}, "device": {Device}, "main_conversation_id": "...",
 "quiet_hours": {"start": "22:00", "end": "07:00"}}
```

The Main chat is created on demand if missing. `quiet_hours` is `null` when
not set.

#### `PUT /api/me/quiet-hours`

```json
{"quiet_hours": {"start": "22:00", "end": "07:00"}}
```

or `{"quiet_hours": null}` to turn them off. Times are `HH:MM` in the
server's local time (`400` otherwise); the window may wrap past midnight.
During quiet hours notifications to the caller are held, not dropped (the
messages still arrive in the app at once); when the quiet hours end they go
out as one digest (see [section 9](#9-push-notifications)). Test pushes are
not affected. Per user. Response: `{"quiet_hours": {...} | null}`.

#### `PUT /api/me`

| Field | Type | Notes |
|---|---|---|
| `display_name` | string | Required, non-blank, max 80. Trimmed. |

Response: `{"ok": true}`.

#### `GET /api/devices`

Lists all of the caller's devices, **including revoked ones** (check
`revoked_at`).

```json
{"devices": [{Device}], "current_device_id": "..."}
```

#### `DELETE /api/devices/{id}`

Revokes one of the caller's own, not-yet-revoked devices (you can revoke the
current device). Also deletes that device's push subscriptions. Response
`{"ok": true}`; `404` if not found, not yours, or already revoked.

#### `GET /api/users` (Owner)

```json
{"users": [{User}]}
```

Ordered by creation time.

#### `POST /api/users` (Owner)

Creates another person (for group conversations). Their Main chat is created
too.

| Field | Type | Notes |
|---|---|---|
| `display_name` | string | Required, non-blank, max 80. Trimmed. |

Response: `{"user": {User}}` with `is_owner: false`.

#### `POST /api/users/{id}/pairing-code`

Owner only. Creates a one-time pairing code for user `{id}`.

```json
{"code": "7KQM-3XPA", "expires_at": 1760000600000}
```

`404` if the user does not exist.

There is no API to delete users or to revoke another user's devices.

### 3.3 Conversations and messages

Conversation kinds: `main` (one per user, cannot be deleted or archived),
`side` (topic chats), `group` (several people), and `job` (hidden; never
returned by the API).

Each member has a role: `owner` for the person who created the conversation
(its **creator**; not necessarily the Iris owner) and `member` for everyone
added later. Only the Iris owner can create group conversations.

A conversation is **shared** when it has more than one member
(`member_count > 1`). A group whose only member is its creator is treated as
personal. Shared conversations change how turns run: the individual-use
policy keeps Claude out unless the owner opts in
([section 7](#7-engine-selection)), and the Iris bridge only offers cards
([section 8](#8-the-iris-mcp-bridge)). In all conversations, bridge tools
act on the creator's data.

The Iris owner's **private context** (`USER.md`, `TOOLS.md`,
`PROACTIVE_PREFERENCES.md`, `MEMORY.md` and the memory notes, write access
to `memory/`, and connectors; only the agent's character in `IDENTITY.md`
and `SOUL.md` is shared) is only given to turns in
a conversation whose single member is the Iris owner, and to scheduled jobs.
Other people's own chats and all shared conversations get none of it, and
there the CLI also runs without the MCP servers, memory, and account
integrations configured in the CLI itself (see the self-hosting guide's
"Not done yet" for what each CLI allows). When
the people in a conversation change (for example a group gets its second
member), the CLI session is restarted rather than resumed, so nothing from
a private session carries over; a member who leaves stops receiving a
running turn's events at once. Membership is read when a turn actually
starts (after any wait for a CLI slot), and a queued message whose sender
has left by then is not answered (its reply ends `cancelled`).

#### `GET /api/conversations`

| Query | Type | Default | Notes |
|---|---|---|---|
| `archived` | bool | `false` | Include archived conversations. |

```json
{"conversations": [{Conversation}]}
```

Ordered with the Main chat first, then by `updated_at` descending (any new
message bumps `updated_at`).

#### `POST /api/conversations`

| Field | Type | Default | Notes |
|---|---|---|---|
| `kind` | string | `"side"` | `"side"` or `"group"`. `group` is **owner-only**. |
| `title` | string | `""` | Max 200. Trimmed. |
| `member_ids` | string[] | `[]` | Other users to add (group only; a side chat with members is `400`). Unknown ids are `400 unknown user <id>`. The caller is always a member. |

Response `200`: `{"conversation": {Conversation}}`. Emits
`conversation.created` to all members.

#### `GET /api/conversations/main`

`{"conversation": {Conversation}}` for the caller's Main chat.

#### `GET /api/conversations/{id}`

```json
{"conversation": {Conversation}, "turn_running": false}
```

`turn_running` is true while a turn holds the conversation (including one
waiting for a free CLI slot).

#### `PATCH /api/conversations/{id}`

| Field | Type | Notes |
|---|---|---|
| `title` | string | Optional, max 200, trimmed. Updates `updated_at`. |
| `archived` | bool | Optional. `true` sets `archived_at`, `false` clears it. Ignored for the Main chat. |

For group conversations only the creator may do this (`403` otherwise).
Archiving applies to every member. Response: `{"conversation":
{Conversation}}`.

#### `DELETE /api/conversations/{id}`

Cancels any running turn, then deletes the conversation and its messages for
every member. The Main chat returns `400` (`the Main chat cannot be
deleted`). For group conversations only the creator may delete (`403`
otherwise; other members can leave instead). Response `{"ok": true}`.

#### `GET /api/conversations/{id}/members`

Any member.

```json
{
  "members": [
    {"user_id": "...", "display_name": "Owner", "role": "owner", "joined_at": 1760000000000},
    {"user_id": "...", "display_name": "Sam", "role": "member", "joined_at": 1760000100000}
  ]
}
```

Ordered by `joined_at`. A member can only read messages (including cards),
attachment references, and activity created at or after their `joined_at`.
The same boundary applies to live WebSocket events and replays, and direct
attachment downloads. Uploaders retain access to their own uploads.
Removing and re-adding a member gives them a new history boundary.

#### `POST /api/conversations/{id}/members`

Adds a user to a group conversation. Only the group's creator.

| Field | Type | Notes |
|---|---|---|
| `user_id` | string | Required. |

Response `{"ok": true}` (also when the user is already a member).

| Status | Cause |
|---|---|
| 400 | Not a group conversation (`only group conversations have members`) |
| 403 | Caller is not the creator |
| 404 | Caller is not a member (`conversation not found`), or unknown `user_id` (`user not found`) |

No event is sent. The new member sees the conversation on their next
conversation list fetch.

#### `DELETE /api/conversations/{id}/members/{user_id}`

Removes a member from a group conversation. Any member may remove
themselves (leave); only the creator may remove someone else. The creator
(role `owner`) cannot be removed. Response `{"ok": true}`.

| Status | Cause |
|---|---|
| 400 | Not a group conversation (`only group conversations have members`), or `{user_id}` is not a member or is the creator (`not a removable member`) |
| 403 | Removing someone else without being the creator |
| 404 | Caller is not a member (`conversation not found`) |

No event is sent.

#### `GET /api/conversations/{id}/messages`

| Query | Type | Default | Notes |
|---|---|---|---|
| `before` | int (ms) | none | Only messages with `created_at` strictly less than this. |
| `limit` | int | 50 | Clamped to 1..200. |

Returns the newest `limit` matching messages in **chronological** order:

```json
{"messages": [{Message}], "has_more": true}
```

To page backwards, pass the `created_at` of the first (oldest) message as
`before`. `has_more` is `true` exactly when older messages exist.
`created_at` is strictly increasing within a conversation.

#### `POST /api/conversations/{id}/messages`

Sends a message and starts an agent turn in the background.

| Field | Type | Notes |
|---|---|---|
| `text` | string | Max 20,000 characters. |
| `attachments` | string[] | Attachment ids from `POST /api/attachments`, max 4. Each must be uploaded by the caller or already appear in a conversation the caller belongs to. |

At least one of non-blank `text` or `attachments` is required.

Response **`202 Accepted`**:

```json
{"user_message": {Message}, "assistant_message": {Message}}
```

`assistant_message` is the placeholder (`status: "streaming"`, empty
`content`) that the turn fills in. See [section 5](#5-turn-lifecycle). The
`message.created` events for both messages are broadcast before the HTTP
response is sent, so they can arrive first.

Sending while a turn is already running in the conversation is allowed; the
new turn waits for the current one to finish.

#### `POST /api/conversations/{id}/cancel`

Cancels the turn currently running in the conversation (a turn still waiting
for a CLI slot is cancelled too).

```json
{"cancelled": true}
```

`false` when nothing was running. The turn then ends with status
`cancelled` (`message.completed` event).

#### `POST /api/conversations/{id}/read`

Sets the caller's read marker for the conversation to now. Response
`{"ok": true}`. (The marker is not currently exposed in any response.)

### 3.4 Attachments

#### `GET /api/attachments`

Images the caller attached to their own messages, for the app's Library,
newest use first, each image once. An upload that was never sent in a chat
(or the agent avatar) is not listed.

| Query | Type | Default | Notes |
|---|---|---|---|
| `limit` | int | 100 | Clamped to 1..500. |
| `before` | int | none | Next page: the previous page's last `used_at`. |
| `before_id` | string | none | With `before`: the previous page's last `id` (photos sent together share `used_at`). |

Ordered by `used_at` (when the photo was last sent), then `id`, newest
first.

```json
{"attachments": [{"id": "...", "mime": "image/jpeg", "size": 48213, "sha256": "<64 hex>",
                  "created_at": 1760000000000, "conversation_id": "...", "message_id": "...",
                  "used_at": 1760000005000}],
 "has_more": false}
```

#### `POST /api/attachments`

Uploads one image as the **raw request body** (not multipart, not JSON; the
`Content-Type` header is not inspected). The type is detected from magic
bytes; only PNG, JPEG, GIF, and WebP are accepted.

| Status | Code | Cause |
|---|---|---|
| 400 | `bad_request` | Empty body |
| 413 | `too_large` | Larger than `max_upload_bytes` (default 10 MiB) |
| 415 | `unsupported_media_type` | Not one of the four image types |

Response `200`:

```json
{"attachment": {"id": "...", "mime": "image/png", "size": 48213,
                "sha256": "<64 hex>", "created_at": 1760000000000}}
```

#### `GET /api/attachments/{id}`

Returns the image bytes with its MIME type, `X-Content-Type-Options:
nosniff`, `Content-Security-Policy: default-src 'none'; sandbox`, and
`Cache-Control: private, max-age=3600`. Visible to the uploader and to members
of any conversation with a message that references it; otherwise `404`.

### 3.5 Feed

Posts come from the For you pipeline (see
[For you pipeline](#for-you-pipeline) below) or from the agent (the
`create_feed_post` bridge tool). Users read, like, delete, and discuss them.

#### `GET /api/feed`

| Query | Type | Default | Notes |
|---|---|---|---|
| `before` | int (ms) | none | Only posts created strictly before this. |
| `limit` | int | 30 | Clamped to 1..100. |
| `liked` | bool | `false` | Only saved (liked) posts. |

```json
{"posts": [{FeedPost}], "has_more": false, "status": {FeedStatus}}
```

Newest first. To page, pass the `created_at` of the last (oldest) post as
`before`. `has_more` is `true` exactly when older posts exist. Within one
day's batch the best-ranked post is the newest.

`FeedStatus` (also from `GET /api/feed/status` as `{"status": {...}}`):

| Field | Type | Notes |
|---|---|---|
| `pipeline` | bool | The daily pipeline runs for this user (the owner; it reads the owner's persona files). |
| `enabled` | bool | `pipeline` and the owner's stored enable choice, falling back to `[feed] enabled` in the config. |
| `running` | bool | A run is in progress. |
| `paused` | bool | Daily posts are paused. |
| `paused_at` | int or null | |
| `paused_reason` | string or null | `unused` (two weeks without opening the feed) or `user`. |
| `daily_size` | int | About how many posts a day (5 to 30, adapts; default 10). |
| `last_batch_at` | int or null | When the latest batch was published (its first post's `created_at`). |
| `last_batch_end_id` | string or null | The latest batch's last (lowest-ranked) post: scrolling past it is `reached_end`. |
| `last_batch_count` | int | |
| `topics` | string[] | Today's topics from the interests call. |
| `sources` | int | Enabled sources. |
| `last_run` | object or null | `{"at", "sources_checked", "items_found", "published", "summarized", "model_used", "requests", "robots_blocked"}`. |

#### `POST /api/feed/activity`

```json
{"event": "opened", "batch_at": 1791700000000}
```

`event` is `opened` (the feed screen was shown, or the app came back to
the front on it) or `reached_end` (the bottom of the `last_batch_end_id`
post came into view); anything else is `400`. Counts are kept per local day (the
day of the batch on screen when `batch_at`, its `last_batch_at`, is given
and within two weeks; else today) and drive the adaptive daily size; either event counts as use for the
two-week pause. Response `{"ok": true}`.

#### `POST /api/feed/pause`, `POST /api/feed/resume`

Pause or resume the daily posts. For the owner, these also persist the app's
enable choice (off/on), overriding the `[feed] enabled` config default.
`enabled=false` in the config means off by default; the owner can turn it on
with Resume (the Daily posts switch). Resuming starts today's batch right away
when there is none yet and Iris is not globally paused. Pausing an active run
stops it at the next phase boundary, with pause checks before publication,
price checks, and urgent notifications. Responses:
`{"ok": true, "status": {FeedStatus}}`, plus `was_paused` for resume. Both
emit `entities.changed` `["feed"]` to the caller.

#### `GET /api/feed/sources`

`{"sources": [{FeedSource}]}`, the user's own first.

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `kind` | string | `rss`, `youtube`, `reddit`, `hn`, `bluesky`, or `mastodon`. |
| `value` | string | Feed URL, channel id or `@handle`, subreddit, `front_page`, Bluesky handle or `at://` feed, `user@instance` or `#tag[@instance]`. |
| `label` | string or null | Display name override. |
| `title` | string or null | The feed's own title, once fetched. |
| `topic` | string or null | Topic it was suggested for. |
| `origin` | string | `user`, `suggested` (by the daily interests call), or `google` (YouTube subscriptions). |
| `enabled` | bool | Suggested sources on arbitrary hosts start off; suggested sources that fail 3 runs in a row turn off by themselves. |
| `last_fetch_at`, `last_ok_at` | int or null | |
| `last_error` | string or null | Short reason, e.g. `HTTP 404`. |
| `fail_count` | int | |
| `created_at` | int | |

#### `POST /api/feed/sources`

```json
{"input": "r/espresso", "topic": "Coffee"}
```

`input` is what a person would type: a website or feed link (a page's
advertised feed is found automatically), `r/name` or a subreddit link, a
YouTube channel link or `@handle` link, a Bluesky handle (`@name.bsky.social`)
or profile/feed link, `@user@instance` or a Mastodon profile link, `#tag`
(Mastodon hashtag), or `hn`. `topic` is optional (max 40). `400` with a
friendly message when the input is not recognized. Adding a source that
already exists makes it yours and turns it on. Response
`{"source": {FeedSource}}`.

#### `PATCH /api/feed/sources/{id}`

`{"enabled": false}`. Response `{"source": {FeedSource}}`.

#### `DELETE /api/feed/sources/{id}`

`{"ok": true}`. Sources you added are deleted. Suggested and YouTube-imported
ones are hidden instead, so later suggestions and imports do not bring
them back; adding the same source by hand restores it.

#### `GET /api/feed/images/{file}`

A photo or icon the pipeline cached from the source (its original image,
never a generated one). `file` is `<32 hex>.<jpg|png|gif|webp|avif|ico>`
and must belong to one of the caller's own posts (else `404`). Sent with `Content-Security-Policy: default-src 'none';
sandbox`, `nosniff`, and `Cache-Control: private, max-age=604800, immutable`.

#### `POST /api/feed/refresh` (Owner)

Runs the feed pipeline now, even if today's batch exists, the feed is
paused (it resumes), or the job is disabled in config. Response **`202`**:
`{"started": true}`; `started` is `false` if a run is already in progress.

#### `GET /api/feed/prompt`

```json
{"feed_prompt": "News, ideas, and finds that fit what I care about ..."}
```

Returns the built-in default when none is set. **Owner only**: the feed
instructions describe the owner's interests.

#### `PUT /api/feed/prompt` (Owner)

| Field | Type | Notes |
|---|---|---|
| `feed_prompt` | string | Required field, max 4,000. Blank resets to the default. |

Response: `{"feed_prompt": "<effective prompt>"}`. Emits `settings.changed`
to all users.

#### `DELETE /api/feed/{id}`

`{"ok": true}`. Emits `entities.changed` with `["feed"]` to the caller.

#### `POST /api/feed/{id}/like`

The body is optional. An empty (or whitespace-only) body or `{}` likes the
post; `{"liked": false}` unlikes it. A non-empty body that is not valid JSON
of this shape is `400` (`invalid JSON body: ...`). `Content-Type` is not
checked.

| Field | Type | Default |
|---|---|---|
| `liked` | bool | `true` |

Response `{"ok": true, "liked": true}`. Emits `entities.changed` `["feed"]`.
Likes raise the post's topic and source in ranking and steer the daily
interests call. Liking a post whose page lists a product price starts a
price watch (see [urgent pushes](#for-you-pipeline)); it stays active while
any liked post links to that product (for example its price-drop post).

#### `POST /api/feed/{id}/discuss`

Opens a side chat about the post, or returns the existing one if the post
already has a discussion the caller can see.

A new conversation gets `seed: {"type": "feed_post", "id": "<post id>"}`, the
post title as its title, and a first message with `role: "system"`,
`kind: "card"`, and

```json
{"type": "feed_post", "title": "...", "body": "...", "url": "<source_url or null>", "ref_id": "<post id>"}
```

The card `body` is the post body cut to 4,000 characters (with `…`
appended when cut); the message `content` (`Feed post "<title>": <body>`,
then `Source: ...`, `Link: ...`, and `Why it was picked: ...` lines when
known) carries the full post and becomes context for the first turn. No turn is
started.

Response `200`: `{"conversation": {Conversation}}`. Emits
`conversation.created` to the caller only when a new discussion is created
(repeat calls return the existing one without an event).

### 3.6 Ideas

Ideas are tasks the agent proposes (`create_idea` bridge tool). Statuses:
`open`, `started`, `done`, `dismissed`.

#### `GET /api/ideas`

| Query | Type | Notes |
|---|---|---|
| `status` | string | Optional filter: one status or several comma-separated (`open,started`); not validated, unknown values match nothing. |

```json
{"ideas": [{Idea}], "has_more": false}
```

Newest first, at most 200; `has_more` says more matched.

#### `PATCH /api/ideas/{id}`

| Field | Type | Notes |
|---|---|---|
| `status` | string | Required, one of the four statuses. |

Response `{"idea": {Idea}}`. Emits `entities.changed` `["ideas"]`.

#### `DELETE /api/ideas/{id}`

`{"ok": true}`. Emits `entities.changed` `["ideas"]`.

#### `POST /api/ideas/{id}/start`

Opens a new side chat (`seed: {"type": "idea", "id": "..."}`, title = idea
title), sets the idea to `started` with its `conversation_id`, and sends the
idea's `prompt` (or `title` plus a blank line and `description` when there is
no prompt) as a user message authored by the caller. No body.

Response **`202`**:

```json
{"conversation": {Conversation}, "user_message": {Message}, "assistant_message": {Message}}
```

Emits `conversation.created` (before the two `message.created` events) and
`entities.changed` `["ideas"]` to the caller. Each call creates a new
conversation.

### 3.7 Goals

#### `GET /api/goals`

| Query | Type | Default |
|---|---|---|
| `include_done` | bool | `false` |

```json
{"goals": [{Goal}]}
```

Open goals first, then newest first; at most 500.

Goals the agent suggested (`suggested: true`, created with the bridge's
`create_goal`) wait for the user to confirm them; the app shows them as
suggestions with "Add goal" (`PATCH {"suggested": false}`) and "No thanks"
(`DELETE`). Suggested goals sort before the other open goals.

#### `POST /api/goals`

| Field | Type | Notes |
|---|---|---|
| `title` | string | Required, non-blank, max 300. |
| `status_text` | string | Optional progress note, max 2,000. |

Goals created here are never suggestions.

Response `{"goal": {Goal}}`. Emits `entities.changed` `["goals"]`.

#### `PATCH /api/goals/{id}`

| Field | Type | Notes |
|---|---|---|
| `title` | string | Optional, non-blank, max 300. |
| `status_text` | string | Optional, max 2,000. |
| `done` | bool | Optional. `true` sets `done_at` to now; `false` clears it. |
| `suggested` | bool | Optional. `false` confirms a suggested goal. |

Response `{"goal": {Goal}}`. Emits `entities.changed` `["goals"]`.

#### `DELETE /api/goals/{id}`

`{"ok": true}`. Emits `entities.changed` `["goals"]`.

### 3.7a Activity and approvals

#### `GET /api/activity`

The tool activity of finished turns in the caller's conversations, newest
first (the app's activity log and "Iris's computer"). The owner also sees
the hidden scheduled-job conversations they own. Turns without tool use are
left out. Only the activity list is returned, never routing, model, or
usage.

| Query | Type | Default | Notes |
|---|---|---|---|
| `before` | int (ms) | none | Only turns created strictly before this. |
| `limit` | int | 50 | Clamped to 1..200. |

```json
{
  "items": [{
    "message_id": "...", "conversation_id": "...", "conversation_kind": "side",
    "conversation_title": "Friday dinner", "status": "complete",
    "activity": [{Activity}], "duration_ms": 41000, "created_at": 1760000000000
  }],
  "has_more": false
}
```

`conversation_kind` is `job` (title `heartbeat` or `feed`) for scheduled
work.

#### `POST /api/messages/{id}/decision`

Approve or deny an [`approval` card](#server-cards). Body `{"approve": true |
false}`. Only the card's `approver_user_id` may decide (`403`), only while it
is `pending` (`409 conflict`, "this was already decided or has expired").
`404` if the caller is not a member of the message's conversation; `400` if
the message is not an approval card. Sets the card's `state` to `approved`
or `denied` and `decided_at`, releases the connector call the gateway is
holding for it (approved: it runs exactly as previewed; denied: it never
runs and the agent is told), broadcasts `message.updated`, and returns
`{"message": {Message}}`.

Owner setup request cards (`requested_by: "system_request"`) also accept
`{"approve": false}` here to decline once (`state: "declined"`). Setup approval
uses the card's existing sign-in/install/connect flow; `approve: true` on a setup
request returns `400`. Every accepted decision atomically inserts a `system_note`
and broadcasts `message.created` alongside the card update. Approval notes record
the decision, not a claim that the held operation succeeded.

### 3.8 Library

Items are saved by the agent through `save_to_library`. Kinds: `artifact`
(documents, notes, code) and `media` (images, audio, video).

#### `GET /api/library`

| Query | Type | Notes |
|---|---|---|
| `kind` | string | Optional exact filter (`artifact` or `media`; not validated). |
| `limit` | int | Default 100, clamped to 1..500. |
| `before` | int | Next page: the previous page's last `created_at`. |
| `before_id` | string | With `before`: the previous page's last `id` (ties on `created_at`). |

```json
{"items": [{LibraryItem}], "has_more": false}
```

Newest first, by `created_at` then `id`.

#### `GET /api/library/{id}`

```json
{"item": {LibraryItem}, "content_url": "/api/library/<id>/content"}
```

#### `GET /api/library/{id}/content`

The stored bytes with the item's `mime`, served with the same safe headers as
attachments (`nosniff`, `default-src 'none'; sandbox` CSP). `404` if the item
or its file is missing.

#### `DELETE /api/library/{id}`

Deletes the item and its file. `{"ok": true}`. Emits `entities.changed`
`["library"]`.

### 3.9 System files and folders

All routes in this section are **owner-only**.

System files are the persona and memory markdown files the agent reads:

| Name | Folder | In chat system prompt | Description |
|---|---|---|---|
| `IDENTITY.md` | persona | yes | Who the agent is: role, boundaries, introduction |
| `SOUL.md` | persona | yes | How the agent communicates and thinks |
| `USER.md` | persona | yes | Facts about you: preferences, goals, people |
| `TOOLS.md` | persona | yes | Notes about tools, accounts and connectors |
| `PROACTIVE_PREFERENCES.md` | persona | yes | When and how the agent acts on its own |
| `HEARTBEAT.md` | persona | no (read by the heartbeat job) | Checklist for scheduled check-ins |
| `MEMORY.md` | memory | `in_prompt: false` (included separately as memory) | Curated long-term memory (always in context) |

#### `GET /api/files`

```json
{
  "system_files": [
    {"name": "IDENTITY.md", "description": "...", "in_prompt": true,
     "exists": true, "size": 1234, "updated_at": 1760000000000}
  ],
  "folders": [
    {"area": "memory", "description": "Long-term memory notes"},
    {"area": "workspace", "description": "The agent's working folder"},
    {"area": "persona", "description": "Persona files"}
  ]
}
```

`size` (bytes) and `updated_at` (file mtime) are `null` when the file does
not exist.

#### `GET /api/files/system/{name}`

```json
{"name": "SOUL.md", "description": "...", "content": "..."}
```

`content` is `""` if the file is missing. `404` for names not in the table.

#### `PUT /api/files/system/{name}`

| Field | Type | Notes |
|---|---|---|
| `content` | string | Required, max 1 MiB (bytes). |

Response `{"ok": true, "name": "SOUL.md"}`. Persona changes apply on the next
turn.

#### Folder routes

`{area}` is `memory`, `workspace`, or `persona` (anything else is `404
folder not found`). The `path` query parameter is relative to the area root.
Paths are rejected (`400`) if they contain `..`, are absolute, contain a
component starting with `.` (hidden), or pass through a symlink.

##### `GET /api/files/browse/{area}?path=<dir>`

`path` defaults to the area root.

```json
{
  "area": "memory",
  "path": "notes",
  "entries": [
    {"name": "projects", "type": "dir", "size": null, "updated_at": 1760000000000},
    {"name": "today.md", "type": "file", "size": 812, "updated_at": 1760000000000}
  ]
}
```

Directories first, then by name. Hidden entries and symlinks are omitted.
`404` if the folder does not exist.

##### `GET /api/files/content/{area}?path=<file>`

```json
{"area": "memory", "path": "notes/today.md", "content": "..."}
```

`400` if not a file, larger than 1 MiB, or not valid UTF-8. `404` if missing.

##### `PUT /api/files/content/{area}?path=<file>`

Body `{"content": "..."}` (max 1 MiB). `path` is required. Parent folders
are created. Response `{"ok": true}`.

##### `DELETE /api/files/content/{area}?path=<file or dir>`

Deletes a file, or an empty folder (`400 folder is not empty` otherwise).
`path` is required. Response `{"ok": true}`.

### 3.10 Settings and avatar

#### `GET /api/settings`

Returns the [Settings](#settings) object directly (not wrapped):

```json
{
  "agent_name": "Iris",
  "avatar_url": "/api/avatar?v=default",
  "avatar_custom": false,
  "allow_claude_for_shared": false,
  "feed_prompt": "...",
  "paused": false,
  "voice": {"enabled": false, "status": "not_implemented"}
}
```

`paused` is true while the owner has paused Iris
([3.18](#318-safety-pause-disconnect-trusted-people)); change it with
`PUT /api/safety`.

#### `PUT /api/settings` (Owner)

All fields optional; only supplied fields change.

| Field | Type | Notes |
|---|---|---|
| `agent_name` | string | Non-blank, max 40, must not contain newline, carriage return, `<`, `>`, or a backtick. Trimmed. |
| `allow_claude_for_shared` | bool | Owner opt-in to let Claude answer in conversations with more than one person ([individual-use policy](#individual-use-policy)). `false` clears the app setting (the config value still applies). |
| `feed_prompt` | string | Max 4,000. Blank resets to the default. |
| `paused` | bool | "Pause Iris". `true` stops every running turn (its CLI process group is killed), closes the browser, and until it is turned off again: chat turns end at once with a short first-person note (error kind `paused`, no push), scheduled jobs do not run, and the browser tools are refused. Saved in the database, so it survives restarts. |

Unknown fields are ignored. Models are no longer set here: pin slots with
[`PUT /api/models/slots/{slot}`](#put-apimodelsslotsslot-owner). (An
`engine` pin stored by earlier versions is read as the `chat` slot's pin.)

Response: the updated Settings object. Emits `settings.changed` to **all**
users (even when the body is `{}`).

#### `PUT /api/settings/avatar` (Owner)

Raw image body, validated exactly like `POST /api/attachments` (PNG, JPEG,
GIF, WebP; `413`/`415` errors). Response: the updated Settings object
(`avatar_custom: true`, `avatar_url` with a new `v`). Emits
`settings.changed`.

#### `DELETE /api/settings/avatar` (Owner)

Reverts to the built-in avatar. Response: the Settings object. Emits
`settings.changed`.

#### `GET /api/avatar`

The current avatar image: the uploaded image, or the built-in SVG
(`image/svg+xml`). The `v` query parameter in `avatar_url` is only a cache
buster. Requires auth.

### 3.11 Harnesses, sign-in, models, and slots

#### `GET /api/harnesses`

```json
{"harnesses": [{HarnessStatus}]}
```

Each entry is a [HarnessStatus](#harnessstatus) plus `label` (e.g.
`"Codex (ChatGPT)"`) and `capabilities` (`{"images", "effort", "mcp",
"resume"}`, all bool). For the owner it also has `login_flow`
(`"code_paste"`, `"device_code"`, or `"manual"`), `login_hint` (the one
server step when the flow is manual), `login` (the current
[LoginStatus](#loginstatus) or `null`), `key_options` (the API keys the
engine accepts: `[{"env", "label", "url", "provider"}]`), and `keys` (names
of the keys saved, e.g. `["GEMINI_API_KEY"]`). `path` is only filled in for
the owner.

#### `POST /api/harnesses/refresh` (Owner)

Re-probes every harness (resolves the binary, runs `--version`, the CLI's
own sign-in check, and its model list where it has one) and returns
`{"harnesses": [...]}`. CLIs without a sign-in check (Gemini CLI) that a
turn marked `auth_required` are reset to `unknown` so the next turn tries
them again. Changed statuses are broadcast as `harness.status`. The server
also probes at startup and every 15 minutes, and a turn that hits a sign-in
error marks the harness `auth_required` immediately.

#### Sign-in from the app

When a CLI the owner has used before goes signed out (probe or turn), Iris
posts one `engine_signin` [card](#card) per outage in the owner's Main chat
("Reconnect Claude"), sends a push notification (`kind: "engine_signin"`),
and says in the card which model it uses meanwhile. Right after an install
from the app, a "Sign in to <engine>" card is posted instead. The card's
`state` follows the sign-in; when the CLI reports it is signed in again the
card becomes `done` (✓).

The sign-in runs the CLI's **own** login command on the server. Iris relays
what the user needs, passes the code the user pastes to the process's
stdin, and judges success only by the CLI's own status command. Codes are
never stored or logged, and Iris never reads the credential files the CLI
writes.

| Harness | `login_flow` | Command | What the user does |
|---|---|---|---|
| `claude` | `code_paste` | `claude auth login --claudeai` | Opens `url`, approves, copies the code shown, sends it with `/login/code`. |
| `codex` | `device_code` | `codex login --device-auth` | Opens `url`, enters `user_code`. Finishes by itself. |
| `gemini` | `manual` | (none; Gemini CLI signs in only in an interactive terminal) | Adds a Gemini API key in the app ([engine keys](#engine-api-keys)), or runs `gemini` on the server once (`login_hint`). |
| `opencode` | `manual` | (none) | Adds a provider key in the app ([engine keys](#engine-api-keys)), or runs `opencode auth login` on the server, or configures a local provider. |

For Gemini CLI and OpenCode the sign-in card uses `flow: "api_key"` with
`key_options`: the app shows a key field (and, for OpenCode, a provider
picker) and saves it with `PUT /api/harnesses/{name}/keys/{env}`.

##### `POST /api/harnesses/{name}/login` (Owner)

Starts the sign-in (or returns the one already running) and returns
`{"login": {LoginStatus}}`. Poll `GET` or listen for `harness.login` until
`state` is `waiting_for_code` (show `url` and a code field) or
`waiting_for_approval` (show `url` and `user_code`). For `manual` harnesses
it re-checks the sign-in instead and returns `{"login": null, "manual":
"<server step>", "auth": "ok" | "auth_required" | "unknown"}`.

##### `GET /api/harnesses/{name}/login` (Owner)

`{"login": {LoginStatus} | null}`.

##### `POST /api/harnesses/{name}/login/code` (Owner)

`{"code": "..."}` (one line, max 4,096 characters). Only accepted while
`state` is `waiting_for_code`; `400` otherwise. Returns `{"login":
{LoginStatus}}` with `state: "verifying"`. A rejected code puts the session
back in `waiting_for_code` with `error` set.

##### `DELETE /api/harnesses/{name}/login` (Owner)

Stops the login process. `{"cancelled": true | false}`.

#### Engine API keys

An engine can also run on an API key the owner adds from the app: how
Gemini CLI and OpenCode providers sign in without a terminal, and an
optional alternative to the subscription for Claude Code and Codex (billed
per use by the provider). Keys are sealed with the vault, never returned or
logged, and passed to that engine's CLI through its documented environment
variable for every turn and probe (after `strip_api_key_env` removed any
inherited ones). Subscription sign-ins are never read or touched.

| Engine | Variables |
|---|---|
| `claude` | `ANTHROPIC_API_KEY` (used for everything while saved) |
| `codex` | `CODEX_API_KEY` (read by `codex exec`; used for everything while saved) |
| `gemini` | `GEMINI_API_KEY` (Iris also selects `gemini-api-key` auth in the per-conversation workspace settings) |
| `opencode` | `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_GENERATIVE_AI_API_KEY`, `GROQ_API_KEY`, `DEEPSEEK_API_KEY`, `MISTRAL_API_KEY`, `XAI_API_KEY`, `OPENCODE_API_KEY` |

##### `GET /api/harnesses/{name}/keys` (Owner)

`{"keys": [{"env", "label", "saved_at"}], "key_options": [{"env", "label",
"url", "provider"}]}`.

##### `PUT /api/harnesses/{name}/keys/{env}` (Owner)

`{"value": "..."}` (one line, no spaces, max 4,096). `env` must be one of the
engine's `key_options` (`400`). Checks the key first with one tiny real
request through the engine's own CLI (for OpenCode, with a model of that
provider), then saves it. Returns the `GET` body plus `auth`: `ok` (saved;
the engine is marked signed in and its sign-in card turns `done` (✓)),
`auth_required` (the provider refused it; **it is not saved**, and a key
saved earlier stays), or `unknown` (could not check, for example the engine
is turned off; saved and used from the next turn). For OpenCode the per-run
config also points each provider at Iris's variable, so a key added in the
app wins over one saved with `opencode auth login`.

##### `DELETE /api/harnesses/{name}/keys/{env}` (Owner)

Removes the key and re-checks the engines. Returns the `GET` body plus
`removed` (bool).

#### `GET /api/models`

Every model the CLIs offer, with availability, plus the slots.

```json
{
  "models": [{ModelEntry, "available": true}],
  "slots": [{Slot}],
  "allow_claude_for_shared": false
}
```

`available` means the model's CLI is enabled, installed, and not signed out
right now. See [section 7](#7-engine-selection).

#### `GET /api/models/slots`

`{"slots": [{Slot}]}` for `chat`, `chat_strong`, `background`, `feed`, and
`classifier`, in that order. A [Slot](#slot) lists only models really
available on this machine, best first, with the recommended one flagged.

#### `PUT /api/models/slots/{slot}` (Owner)

```json
{"mode": "pinned", "model": "codex:gpt-6.1-sol", "effort": "medium"}
```

or `{"mode": "automatic"}`. When pinning, `model` must be a catalog id that
is available right now (`400` otherwise) and `effort`, if given, one of its
`efforts`. Unknown slot: `404`. Returns `{"slots": [...]}` and broadcasts
`models.changed`. A pinned slot never changes by itself; if the pinned model
stops being usable, turns fall back to the slot's recommendation (recorded
as `router: "fallback"`) and the slot shows `pin_unavailable: true`.

### 3.12 Connectors

Connectors are MCP servers the owner connected (Gmail, GitHub, any MCP
server). **They are never handed to the CLI.** Each turn gets one small
stdio proxy per usable connector (`iris mcp-gateway`, under the connector's
name, so tools still appear as `<connector>/<tool>`), and the proxy forwards
`tools/list` and `tools/call` to the Iris daemon over a private Unix socket
(`<data dir>/run/gateway.sock`, 0600) with a token valid only for that
turn. The daemon runs the real server: a stdio program it starts with the
connector's secrets in the environment (working directory
`<data dir>/connectors/<id>`, outside the agent's workspace), a remote
Streamable HTTP server it calls with the connector's headers or OAuth token,
or a program that serves HTTP on a loopback port (`{port}` in its arguments,
environment, and URL is replaced with a free port). The agent never sees a
connector secret, and Iris never decides what to call: it forwards the
CLI's calls and enforces the owner's rules on them.

Who gets them: turns in the **owner's** own conversations (the owner is the
only member) and the owner's scheduled jobs, for connectors that are on,
not paused, and signed in. All routes are **owner-only**. Shared
conversations (more than one member) and other people's conversations get
none. Programs stop after `safety.idle_minutes` without use and start again
on the next call. Connectors saved before the gateway existed keep running
in the workspace folder (they may use paths relative to it); new ones run
in a private folder.

MCP servers configured in a CLI itself (outside Iris) are turned off in
every Iris turn, the owner's included, so nothing bypasses the gateway:
Claude Code runs with `--strict-mcp-config`, Gemini CLI with
`--allowed-mcp-server-names`, and Codex and OpenCode get `enabled = false`
for the servers their own listings report (`codex mcp list --json`,
`opencode debug config`; names only, read before each turn). If that
listing fails, the engine is skipped for the turn rather than run with
those servers on. Where a CLI's own server has the same name as one of
Iris's, Iris's is passed as `iris_<name>` so the two are never merged. Add
such services as connectors instead.

Connector descriptions, results, and diagnostics redact known secret values.
For secrets of at least eight characters, this also covers standard and
URL-safe base64 (padded or unpadded), hex, URL encoding, reversal, and
case-insensitive echoes. **Redaction is best-effort**: arbitrary transforms,
splitting, or combinations can evade it. The approval gate remains the
boundary for connector actions; redaction is not an authorization mechanism.

**The approval gate.** Every call is classified from the real tool name,
the server's own annotations (`readOnlyHint`, `destructiveHint`; risky words
in the name win), and the real arguments: `read`, `draft`, `write`, `send`
(email, messages, posts, replies, sharing, invites, approving or merging,
and creating or changing things others see such as pull requests, issues,
releases, and comments),
`purchase` (buying, paying, ordering, booking), `delete`, `export`, or
`unclear`. Unrecognized tool names are `unclear` and ask, including when
the server supplies a read-only hint; an allow override also still asks
when the category is unclear. Concatenated delivery verbs and names
such as `dispatch_message`, `wire_money`, and `sync_to_remote` retain their
risk. A draft exemption requires a leading draft/compose verb (or create
with a draft noun), and no delivery verb anywhere. Operation arguments of multi-purpose tools (`method`, `action`,
`operation`, `op`, `verb`, `command`, `event`) count too: the riskier
reading wins, and an operation Iris cannot place on a tool that changes
things is `unclear`. Recipients are read from
the arguments (`to`, `cc`, `bcc`, `recipients`, `attendees`, `email`,
`phone`, `channel`, ...), including guest-email and invite arrays. For
send/share/write calls, address-like strings under unknown keys are also
recipients. Ambiguous values or nesting beyond the scan limit add an
unreadable-audience placeholder that asks. Reply-all flags accept truthy
booleans, numbers, and strings (`yes`, `1`, `TRUE`, with whitespace);
public visibility, anyone access, and link sharing add a public audience.
With the default rules:

| Call | What happens |
|---|---|
| read, draft | Runs. |
| write | Runs, unless it reaches someone new (a recipient not trusted and never approved before), which asks. |
| send | Runs without asking only when every recipient is a [trusted person](#318-safety-pause-disconnect-trusted-people) and the server cannot add others (a reply-all flag counts as "everyone else on the thread"; publishing a pull request, issue, comment, or post counts as "everyone who can see it", whoever is assigned); otherwise asks. Sending, publishing, or forwarding an existing draft is a send; composing one is a draft. |
| purchase | Always asks, whatever the rules. |
| delete, export, unclear | Asks. |

Asking means: Iris holds the call, posts an [`approval` card](#server-cards)
built from the exact arguments in the turn's chat (the owner's Main chat for
scheduled jobs), sends a push (`kind: "approval"`), and waits. Approved: the
held call runs unchanged and its recipients become known. Denied, no answer
within `safety.approval_timeout_secs` (default 300, and at most a minute
less than `turn_timeout_secs`; the card becomes `expired`), or the turn
ended, Iris was paused, or the connector was turned off, removed, or paused
(the card becomes `cancelled`): it never runs, and the agent gets an error
result saying so. Stopping a turn, or someone joining its chat, ends its
connector access at once (before the CLI has finished exiting). Right
before any call runs (and again after starting the connector), Iris checks
that the connector still exists, is on, unpaused, and signed in, that it is
the same target and account the owner approved, that its current rules and
trusted people still allow the call, that the turn is still running, that
its chat is still the owner's alone, and that Iris is not paused. Editing a connector's
settings or signing it in again cancels its waiting approvals. A request too large to show in full on the card (over 64,000
characters) is refused rather than approved unseen. Tools that would ask are marked in their description; tools the
rules turn off are left out of `tools/list`.

**Google: rules per service and action.** Google is one connection with
one sign-in for every scope, so what Iris may do in each Google service is
enforced here, in the gateway. Every tool of `workspace-mcp@2.1.0` belongs
to a service (`gmail`, `calendar`, `drive`, `docs`, `sheets`, `slides`,
`forms`, `tasks`, `contacts`; `youtube` has no tools: it is what For you
reads) and performs one or more friendly actions, each with a rule key such
as `gmail.send` or `calendar.events.invite`. Multi-purpose tools are placed
by their real arguments: `manage_event` with `action: "create"` is
"Create events", or "Create events with invitations" when it has
`attendees`; `action: "update"` is "Update events and notify guests" unless
`send_updates` is `"none"` and no attendees are given (Google tells guests
by default), and deleting also notifies guests unless `send_updates` is
`"none"`; `update_drive_file` with `trashed: true` is "Move files to the
trash", with new `content`/`file_url`/`file_path` (unless `mode` is
`append` or `prepend`) it replaces the file (a deletion for the starting
rules), and with sharing settings or `add_parents` (moving into a folder
gives its people access) also "Share files, change who has access, or put
them in folders", as is creating, copying, or importing a file (Drive, Docs,
Sheets) into a folder other than My Drive's root; populating a Docs tab
replaces its content (a deletion) unless `replace_existing` is `false`; Gmail labels that add
`TRASH` are "Move email to the trash" (other label changes in the same call
are also "Label, archive and mark email"); filters with a `forward` or
`TRASH` action also need "Filters that forward email to someone" or "Move
email to the trash", as does updating or applying a filter without giving
its action, or applying one by `filter_id` (the server keeps or applies the
saved one, whatever action is given); applying a filter also changes
labels on existing mail; a one-action tool the server marks destructive
(`batch_update_doc`, `resize_sheet_dimensions`...) counts as a deletion for
the starting rules; comments on Docs, Sheets and Slides
are their own "Comment on ..." actions. Arguments are read the way the
server reads them (it accepts `addLabelIds` for `add_label_ids`, JSON
strings for objects and lists, and strips and lowercases `action`). A filter create or update without its action keeps a saved forwarding
destination the call does not show, so it is never treated as going only
to trusted people. A call
that performs several actions needs every one of them allowed. A tool or argument value the table
does not know (`action: "purge"`, a newer server's tool) is classified as
above under its tool name, which asks for anything it cannot place. The
owner's rule for an action (`allow`, `ask`, `block`) is stored in the
connector's `policy.tools` under the action key; without one, the
connection's mode decides (Google starts `careful`: reads run, every
change, send, or deletion asks, sends only to trusted people run). `block`
on an action refuses calls that perform it; when every action a tool can
perform is blocked, the tool is also left out of `tools/list` and refused
whatever its arguments. Turning a service off (`policy.off`) hides and
refuses all of its tools, whatever the action rules say, and turning
`youtube` off stops For you from reading YouTube. Rules saved by an older
Iris are read in this format: a rule on a Google tool becomes the rule of
every action it performs (the strictest wins when tools share an action),
and a Google connection still on the old `standard` default moves to
`careful`.

**Tripwires.** Checked and recorded in one step right before a call runs
(so concurrent calls, or several approvals released together, cannot all
pass), counting calls that ran in the last hour through one connector: more than `safety.max_sends_per_hour` sends (default 5), more
than `safety.max_calls_per_hour` calls (200), or more than
`safety.max_read_mb_per_hour` MB of results (25, a bulk export) pauses the
connector: the call is refused, an `action` card "I paused <service>" with a
Resume button is posted in the Main chat, and the owner gets a push (`kind:
"safety"`). Calls already admitted finish; waiting approvals for that
connector are cancelled. The data limit is also checked right after each
result. The hour counts from when calls ran, not when they were asked. Every call (ran, asked, refused, including calls refused because the
connector was paused, off, or signed out) is recorded in the
[connector activity log](#get-apiconnectorsactivity).

**Sign-in (OAuth 2.1 + PKCE).** A connector with `auth: "oauth"` signs in on
the service's own page:

- `discover` (custom remote MCP servers, GitHub, one-tap catalog services):
  Iris follows the MCP
  authorization spec: the 401's `WWW-Authenticate` `resource_metadata`, or
  `/.well-known/oauth-protected-resource[/<path>]` (RFC 9728), then the
  authorization server's metadata (RFC 8414 path insertion, then OpenID
  discovery), requires S256 PKCE support, registers itself with dynamic
  client registration (RFC 7591) when offered, and sends the RFC 8707
  `resource` parameter. Without registration the owner pastes the client
  ID (and secret) of an OAuth app they created (`409 needs_client`).
  Every address Iris fetches while signing in must be https, must not
  resolve to a loopback, private, or link-local address, and is not
  followed through redirects (loopback is allowed only when the connector
  itself is on this machine).
- `preset` (Google): fixed endpoints and scopes, the owner's own OAuth
  client (`access_type=offline`, `include_granted_scopes=true`), requesting
  in one sign-in `gmail.modify`, `gmail.settings.basic` (Gmail filters),
  `calendar`, `drive`, `documents`,
  `spreadsheets`, `presentations`, `forms.body`, `forms.responses.readonly`,
  `tasks`, `contacts`, and `youtube.readonly` (plus `openid`, email and
  profile). A sign-in that does not grant every one is refused. The
  connector's `oauth.scopes` records the last full grant; a Google
  connection granted before a scope was added keeps working and reports
  `oauth.needs_more_access: true` until the owner signs in again, which
  requests every current scope.

Google requires an HTTPS `public_url` and always uses its exact
`<public_url>/api/oauth/callback`, regardless of the requesting app origin.
For other connectors, the redirect URI is `<base>/api/oauth/callback`, where base is `public_url`
or the app's origin. If that is not https (and not this machine), the
redirect goes to `http://127.0.0.1:<port>/api/oauth/callback` instead; the
browser lands on a page that does not load, and the owner pastes that
address into the app (`POST /api/oauth/complete`). Tokens are sealed with the
vault, refreshed a minute before they expire, and sent only to the
connector. A refused token (401/403 at startup, while listing, or on a
call, after one refresh and retry, or a refused refresh) marks the
connector `needs_reconnect` and posts a `connector_reconnect` card (`auth:
"oauth"`) whose button signs in again. A sign-in server that cannot be
reached or is having trouble (HTTP 429/5xx, `temporarily_unavailable`)
fails only that call. A new sign-in replaces the previous grant entirely;
changing a connector's web address clears its sign-in. A sign-in whose code
exchange was overtaken by a newer change (another sign-in, a new client, a
new address, or setting it up again) is refused instead of saving its
tokens. Private addresses are allowed during sign-in only when every
address the connector's host resolves to is private. Tool error results
are scrubbed of the connector's secrets before the agent sees them.

Secret values (`env` values, `headers` values, OAuth client secrets and
tokens) are encrypted at rest and **never returned**; responses list only
key names. See [Connector](#connector).

#### `GET /api/connectors`

`{"connectors": [{Connector}]}`, ordered by name.

#### `POST /api/connectors`

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | | Required. 1 to 48 characters of `A-Z a-z 0-9 _ -`, not `iris` (reserved). Unique (`409 conflict`). Used as the MCP server name and tool prefix. |
| `description` | string | `""` | Max 500. |
| `transport` | string | `"stdio"` | `"stdio"` or `"http"`. |
| `command` | string | | stdio: required, max 500. http: optional; a program Iris starts that serves the URL (use `{port}`). |
| `args` | string[] | `[]` | Program arguments. |
| `env` | object | `{}` | Program environment, `{"KEY": "secret value"}`. |
| `url` | string | | http: required. Must start with `http://` or `https://`, max 2,000. |
| `headers` | object | `{}` | http headers, `{"Authorization": "Bearer ..."}`. |
| `auth` | string | `"none"` | http only: `"oauth"` signs in with the MCP authorization spec (the connector starts as `needs_sign_in`; continue with `POST /api/connectors/{id}/oauth/start`). |
| `enabled` | bool | `true` | |

Keys in `env` and `headers` must be 1 to 100 characters of `A-Z a-z 0-9 _ -`.
HTTP connectors with OAuth or saved headers require an `https://` URL.
Plain HTTP is allowed only for `localhost` or a loopback IP (such as
`127.0.0.1` or `[::1]`). Creation and configuration updates reject unsafe
addresses; the gateway also refuses to attach credentials to unsafe
addresses saved by older versions. OAuth discovery follows the same rule.

```json
{
  "name": "calendar",
  "description": "Calendar access",
  "transport": "stdio",
  "command": "npx",
  "args": ["-y", "some-calendar-mcp"],
  "env": {"API_TOKEN": "..."}
}
```

Response `{"connector": {Connector}}`.

#### `PATCH /api/connectors/{id}`

Same fields except `name`, `transport`, and `auth`, which cannot be changed
(they are ignored if sent). Supplying `env` or `headers` **replaces** the
whole map. Changing the configuration restarts the connector and closes its
reconnect card. Response `{"connector": {Connector}}`.

#### `POST /api/connectors/{id}/connect`, `POST /api/connectors/{id}/disconnect`

Set `enabled` to `true` / `false` (turning it off stops its program).
Response `{"connector": {Connector}}`.

#### `POST /api/connectors/{id}/resume`

Turns the connector on and clears a tripwire pause. Response
`{"connector": {Connector}}`.

#### `PUT /api/connectors/{id}/policy`

`{"mode"?: "standard" | "careful" | "read_only" | "ask_always", "tools"?:
{"<tool or action>": "allow" | "ask" | "block" | null}, "services"?:
{"<google service>": true | false}}`. `null` removes an override. For
Google, rules on tools the [action table](#connectors) knows are keyed by
action (`gmail.send`); `services` turns a Google service on or off (only
for a Google connection; unknown services are `400`). Modes, in plain words: `standard` "Reads freely · asks before
sending, buying, or deleting" (default), `careful` "Reads freely · asks
before any change", `read_only` "Only reads · never changes anything",
`ask_always` "Asks every time". A tool set to `allow` runs without asking,
except purchases and unclear actions, which always ask. Response `{"connector": {Connector}}`.

#### `GET /api/connectors/{id}/tools`

Starts the connector if needed and lists its tools:
`{"tools": [{"name", "label", "description", "category", "rule", "actions",
"service", "effect"}], "error": null}`, where `effect` is `runs`, `asks`,
`asks unless trusted`, or `off`. For Google, `actions` lists the action
keys the tool can perform (their rules apply instead of `rule`) and
`service` its Google service; both are empty/null otherwise. If it cannot
start: `{"tools": [], "error": "...", "detail": "<last line the program
printed, or null>"}`.

#### `GET /api/connectors/{id}/permissions`

A service's permission screen. `?service=<google service>` (Google only;
`404` otherwise) lists that service's actions from the action table without
starting the server. Without it, the tools the server lists (starting it if
needed; for Google only tools that belong to no service):

```json
{
  "service": {"id": "calendar", "label": "Google Calendar",
              "description": "Manage calendar events and scheduling", "on": true},
  "account": "owner@example.com",
  "sections": [
    {"id": "read", "label": "Read permissions", "actions": [
      {"id": "calendar.events.read", "label": "See events", "description": "",
       "category": "read", "rule": null, "value": "allow", "default": "allow",
       "choices": ["allow", "ask", "off"], "note": null}]},
    {"id": "write", "label": "Write and delete permissions", "actions": [
      {"id": "calendar.events.invite", "label": "Create events with invitations",
       "category": "send", "rule": null, "value": "ask", "default": "ask",
       "choices": ["allow", "ask", "off"],
       "note": "Asks unless it only reaches people you trust"}]}
  ],
  "error": null, "detail": null
}
```

`service` is null without `?service`. `account` is the account the
service reported when signing in (the OpenID email), or null. `rule` is the
owner's override (`allow`, `ask`, `block`) or null; `value` is what happens
now and `default` what happens without the override (`allow`, `ask`, `off`).
Purchases and actions Iris cannot place leave `allow` out of `choices`
(they always ask). A `note` also says when the starting rules treat the
kinds of call a grouped action covers differently. The app saves every
choice as an explicit rule (choosing Ask over an "asks unless trusted"
default stores `ask`, which asks even for trusted people). Sections without actions are left out. If the server
cannot start: `"sections": [], "error": "...", "detail": "..."`.

#### `POST /api/connectors/{id}/oauth/sign-out`

Forgets a signed-in connector's tokens and account (`auth_state` becomes
`needs_sign_in`); its OAuth client, endpoints, and rules stay, so
connecting again is one sign-in. Cancels its waiting approvals and sign-ins
under way and stops its server. Iris does not revoke the grant at the
provider. `400` for a connector without OAuth. Response
`{"connector": {Connector}}`.

#### `GET /api/connectors/catalog`

`?origin=<the app's origin>`. The services Iris knows how to set up, and
the same services by group as the app lists them:

```json
{
  "services": [{
    "id": "google", "label": "Google", "description": "Gmail, Calendar, Drive, Docs, Sheets, Slides, Forms, Tasks, Contacts, and YouTube read-only",
    "setup": "google_client", "group": "productivity", "missing": [], "connector_id": null, "connected": false,
    "redirect_uri": "https://iris.example.ts.net/api/oauth/callback", "redirect_direct": true
  }],
  "groups": [{
    "id": "productivity", "label": "Productivity",
    "rows": [{"service": "google", "part": "gmail", "label": "Gmail",
              "description": "Read, organize, draft and send email",
              "connector_id": null, "state": "not_connected"}]
  }],
  "modes": [{"mode": "standard", "label": "Reads freely · asks before sending, buying, or deleting"}]
}
```

`groups` are `productivity`, `development`, `money`, and `lifestyle`, in
that order, each only when it has rows; rows are in display order (the app
shows the first five and a "See all"). Google is listed as its services
(`part`: `gmail`, `calendar`, `drive`, `docs`, `sheets`, `slides`, `forms`,
`tasks`, `contacts` under Productivity, `youtube` under Lifestyle), all
backed by the one Google connector. `state` is `not_connected`,
`needs_sign_in`, `off` (the connection, or that Google service, is off),
`paused`, or `connected`. `custom` is in no group.

`setup` says how the owner connects it: `google_client` (the Google setup
wizard), `token_or_oauth` (GitHub), `oauth_sign_in` (one tap: the service's
own hosted MCP server, signed in with discovery and dynamic client
registration), or `custom`. `missing` lists programs the service needs on
the server (Google needs `uvx`). `redirect_uri` is the address to register with the service;
`redirect_direct: false` means the owner will paste the final address back
for other services. Google instead reports `redirect_uri: null` until an HTTPS
`public_url` is configured.

#### `POST /api/connectors/catalog/{service}`

Creates (or updates) the connector named after the service. Response
`{"connector": {Connector}, "needs_sign_in": bool}`.

- `google`: `{"client_id", "client_secret"}` from the owner's Google Cloud
  project (a "Web application" OAuth client; the client ID must end in
  `.apps.googleusercontent.com`). Iris runs the open-source Google
  Workspace MCP server (`uvx workspace-mcp@2.1.0 --transport streamable-http
  --tools gmail calendar drive docs sheets slides forms tasks contacts`) on a
  loopback port in its external OAuth mode and passes the owner's access
  token with every request. The server's other services stay off: `chat`
  (its API needs a Business or Enterprise Workspace account and a
  configured Chat app), `search` (the Custom Search JSON API is closed to
  new customers and needs its own key and engine), and `appscript` (running
  arbitrary scripts in the owner's account would step around per-call
  approval). No
  client secret ships with Iris. Saving the same key again keeps the current
  sign-in and its recorded scopes. When a Google sign-in completes, server
  arguments that are exactly an older Iris default (`--tools gmail calendar
  drive`) are replaced with today's; arguments the owner changed are kept.
- `github`: `{"token"}` (a personal access token, sent as a bearer header to
  `https://api.githubcopilot.com/mcp/`), or `{"client_id",
  "client_secret"}` of the owner's GitHub OAuth app (then sign in).
- One-tap services (`setup: "oauth_sign_in"`): `{}`. Iris saves an `http`
  connector for the vendor's hosted MCP server with `auth: "oauth"` in
  `discover` mode (exactly what `POST /api/connectors` stores for a custom
  server with `auth: "oauth"`) and returns `needs_sign_in: true`; continue
  with `POST /api/connectors/{id}/oauth/start`, which registers Iris with the
  service and returns its sign-in page. A new connection starts with the
  `careful` rules (reads run; every change, send, or deletion asks); money
  services (`stripe`, `mercury`) start with `ask_always` (every action,
  reads included, asks). Setting one up again while it still points at the service's server
  changes nothing (sign-in, rules, and a sign-in under way are kept) and
  reports whether it still needs signing in.

  | `service` | Group | Server |
  |---|---|---|
  | `notion` | Productivity | `https://mcp.notion.com/mcp` |
  | `todoist` | Productivity | `https://ai.todoist.net/mcp` |
  | `airtable` | Productivity | `https://mcp.airtable.com/mcp` |
  | `monday` | Productivity | `https://mcp.monday.com/mcp` |
  | `miro` | Productivity | `https://mcp.miro.com/` |
  | `canva` | Productivity | `https://mcp.canva.com/mcp` |
  | `calcom` | Productivity | `https://mcp.cal.com/mcp` |
  | `granola` | Productivity | `https://mcp.granola.ai/mcp` |
  | `fireflies` | Productivity | `https://api.fireflies.ai/mcp` |
  | `zapier` | Productivity | `https://mcp.zapier.com/api/v1/connect` |
  | `linear` | Development | `https://mcp.linear.app/mcp` |
  | `sentry` | Development | `https://mcp.sentry.dev/mcp` |
  | `cloudflare` | Development | `https://mcp.cloudflare.com/mcp` |
  | `supabase` | Development | `https://mcp.supabase.com/mcp` |
  | `netlify` | Development | `https://netlify-mcp.netlify.app/mcp` |
  | `neon` | Development | `https://mcp.neon.tech/mcp` |
  | `huggingface` | Development | `https://huggingface.co/mcp?login` (without `?login` it answers anonymously) |
  | `stripe` | Money | `https://mcp.stripe.com` |
  | `mercury` | Money | `https://mcp.mercury.com/mcp` |
  | `readwise` | Lifestyle | `https://mcp2.readwise.io/mcp` |

  Each address was checked live: an unauthenticated request gets 401,
  discovery finds the sign-in server's metadata with a registration
  endpoint, registration with Iris's exact request succeeds for both an
  https and a loopback callback, and the sign-in page loads. Services that
  failed are not in the catalog: their registration refuses Iris or only
  allows listed callbacks (Vercel, Square, Dropbox, Intercom, Figma,
  ClickUp's documented allowlist, Calendly for loopback), they publish no
  registration endpoint (Box, Asana, HubSpot, Spotify, Instacart's shopping
  server), they need no sign-in (Instacart's recipe server), or their
  documented address does not answer (PayPal). Connect them as a custom
  connection with your own OAuth app.

Tools of a catalog service are classified without the service's own name
in front (`notion-search` reads like `search`); per-tool rules and the
activity log keep the real name.

#### Google setup wizard (owner only)

`GET /api/connectors/google/setup` returns:

```json
{
  "progress": {
    "step": 3, "total_steps": 6, "phase": "setup", "more_access": false,
    "summary": "Google setup: step 3 of 6", "project_id": "example-project-123456", "error": null
  },
  "pages": {
    "project": "https://console.cloud.google.com/projectcreate",
    "apis": "https://console.cloud.google.com/flows/enableapi?apiid=gmail.googleapis.com,calendar-json.googleapis.com,drive.googleapis.com,youtube.googleapis.com,docs.googleapis.com,sheets.googleapis.com,slides.googleapis.com,forms.googleapis.com,tasks.googleapis.com,people.googleapis.com&project=example-project-123456",
    "branding": "https://console.cloud.google.com/auth/branding?project=example-project-123456",
    "audience": "https://console.cloud.google.com/auth/audience?project=example-project-123456",
    "client": "https://console.cloud.google.com/auth/clients/create?project=example-project-123456"
  },
  "redirect_uri": "https://iris.example/api/oauth/callback",
  "authorized_domain": "iris.example"
}
```

Steps 1–6 are project, APIs, branding/audience, login key, publish, paste.
`phase` is `setup`, `sign_in` (after the key is saved), or `connected`.
`more_access` is true when connected with a grant from before Docs,
Sheets, Slides, Forms, Tasks and Contacts: the app asks the owner to turn
on the new APIs (`pages.apis` turns on all ten; `pages.api_docs`,
`api_sheets`, `api_slides`, `api_forms`, `api_tasks`, `api_people` and the
older `api_gmail`, `api_calendar`, `api_drive`, `api_youtube` open one
each) and sign in again.
The Console URLs are defined together on the server. All later Console links,
including the individual API fallbacks, are pinned with `project=<project_id>`
when an ID is saved; otherwise they retain their unpinned URLs. `redirect_uri` is null
until `public_url` is a valid HTTPS URL without credentials, query or fragment.
`authorized_domain` is the domain to enter in Branding's Authorized domains,
without scheme, port or path. Iris uses a small embedded public-suffix subset
(including `ts.net`, common TLDs and country suffixes such as `co.uk`) to derive
the registrable domain. For unknown suffixes it removes the first label of
hosts with three or more labels, otherwise showing the full host. It is null
without a valid HTTPS callback or DNS host. Steps 3 and 4 link to HTTPS setup help.

`PUT /api/connectors/google/setup` accepts `{"step": 1..6, "sign_in"?: false,
"project_id"?: "..."}`. The optional non-secret project ID accepts a bare ID,
a Console URL containing `project=<id>`, or copied text containing the ID.
It must be 6–30 lowercase ASCII letters, digits or hyphens, start with a
letter and not end with a hyphen. Invalid or ambiguous input returns a friendly
400 without changing progress. The saved ID can be replaced at any step;
omitting it preserves it through navigation, sign-in and errors. Older saved
progress without an ID still loads (`project_id: null`).
It saves progress in SQLite and clears the previous reported error. Passing
`sign_in: true` requires step 6 and a saved Google client. Advancing beyond
step 4 or returning to sign-in requires HTTPS `public_url`. Returns the same
object as GET. Done is the owner's confirmation, not a Google Console check.

`POST /api/connectors/google/setup/client` accepts `{"paste": "...",
"validate_only"?: false}`. The paste is capped at 64 KiB and accepts downloaded
`web`/`installed` client JSON, or an ID ending in `.apps.googleusercontent.com`
and a secret starting with `GOCSPX-`, with labels, whitespace and either order.
Multiple distinct keys, missing parts and malformed values get friendly 400
errors. `validate_only: true` returns only `{"valid": true}` and stores nothing.
Saving requires HTTPS, uses the existing single Google connector and vault,
then moves progress to sign-in. Returns the same object as GET; neither path
returns the extracted ID or secret. The frontend validates locally on input
and clears its paste after saving or leaving the step; nothing is stored there.

`POST /api/connectors/google/setup/error` accepts `{"code": "..."}` for an
error Google showed before it could return to Iris. Only allowlisted codes
are accepted. Callback and token-exchange errors also update progress:
`redirect_uri_mismatch` → step 4, `access_denied` → step 3 (retry if cancelled),
`invalid_client` → step 6 (create a new key in step 4 if deleted),
`unauthorized_client` → step 4, `invalid_scope` → step 2.
A partial Google grant reports `missing_permissions` → sign-in at step 6;
it is not stored as a completed connection.
The response has `progress.error: {"step", "message"}` with a fixed, plain
explanation. Provider prose and pasted credentials are never saved in progress
or echoed. Successful sign-in clears the error, resolves setup cards and posts
a private system note, including when setup was opened from Connections.
Progress mutations broadcast `connectors.changed`; clients refetch GET.

#### `POST /api/connectors/{id}/oauth/start`

`{"origin": "<the app's origin>"}`. Discovers and registers as needed and
returns `{"authorize_url", "redirect_uri", "paste": bool}`. Turning the
connector off or removing it drops a sign-in in progress. The app opens
`authorize_url` in the owner’s real browser, using Safari schemes in an iOS
standalone PWA and `target="_blank" rel="noopener noreferrer"` elsewhere, with
a Copy link fallback (also used for engine sign-in and Google Console links); when the owner approves, the service sends the browser to
`GET /api/oauth/callback`, or (when `paste`) the owner pastes the address
they landed on into `POST /api/oauth/complete`. `409 needs_client` when the
service has no dynamic registration and no client is saved (the message
names the callback address to register). A new start replaces an unfinished
one for the same connector.

#### `POST /api/connectors/{id}/oauth/client`

`{"client_id", "client_secret"?}`: the owner's own OAuth client for a
`discover` connector. Google replacements use the wizard’s single-paste
endpoint so its local server and vault credentials stay synchronized. Clears saved tokens. Response `{"connector":
{Connector}}`.

#### `GET /api/oauth/callback`

`?state&code` (or `?state&error`). No device token. Finishes the sign-in the
`state` belongs to (one use, 15 minutes): exchanges the code with the PKCE
verifier, stores the tokens, marks the connector `ok` (it never turns a
connector on by itself), closes its reconnect card, and broadcasts
`connectors.changed`. Answers with a small
HTML page ("Google is connected. You can close this page and go back to
Iris.", or what went wrong).

#### `POST /api/oauth/complete`

`{"url": "<the address the browser landed on>"}` (max 8,000). Same as the
callback. Response `{"connected": "<service label>"}`; `400` with a friendly
message otherwise.

#### `GET /api/connectors/activity`

`?connector_id&before&limit` (limit 1..200, default 50). Every gated call,
newest first: `{"items": [{ConnectorCall}], "has_more": bool}`.

#### `DELETE /api/connectors/{id}`

Stops it and deletes it with its saved secrets, tokens, and working
directory. `{"ok": true}`.

### 3.13 Credentials

Saved logins and payment cards. All routes are **owner-only** and scoped to
the caller. Secrets (password, notes, card number, CVC) are encrypted at rest
and **never returned**; responses only contain non-secret metadata.

The agent uses them without seeing them: its `fill_saved_login` and
`fill_saved_card` tools (see [the browser tools](#the-browser-tools)) name the
fields, and Iris types the saved values in itself; every card use needs the
owner's OK on an approval card. While the owner has taken over Iris's
browser, the app can also fill one field at a time into the page
(`browser.input` with `kind: "fill"`, see [the
WebSocket](#4-websocket-get-apiws)). Either way the value never reaches a
tool result, a message, a client, or a log. A login is filled only into the site
it was saved for (scheme, host, and port must match the saved `site`;
subdomains require `allow_subdomains: true`), and anything only over HTTPS
(plain HTTP only on this machine). `last_used_at` records the last fill.

#### `GET /api/credentials`

```json
{"credentials": [{Credential}]}
```

Ordered by kind, then label.

#### `POST /api/credentials`

Common fields:

| Field | Type | Notes |
|---|---|---|
| `kind` | string | Required: `"login"` or `"card"`. |
| `label` | string | Required, non-blank, max 200. |

Login fields:

| Field | Type | Stored as | Notes |
|---|---|---|---|
| `site` | string | metadata | Max 300. Full origin; bare domains mean HTTPS on port 443. |
| `allow_subdomains` | bool | metadata | Optional, default false. Allows subdomains only on the same scheme and port. |
| `username` | string | metadata | Max 300. |
| `password` | string | secret | Max 1,000. |
| `notes` | string | secret | Max 4,000. |

At least one of `password` or `notes` is required when creating
(`400 password or notes is required`).

Card fields:

| Field | Type | Stored as | Notes |
|---|---|---|---|
| `number` | string | secret (`last4` in metadata) | Required when creating (`400 number is required`). Non-digits are stripped; must be 12 to 19 digits and pass the Luhn check. |
| `cvc` | string | secret | 3 or 4 digits. |
| `exp_month` | int | metadata | 1 to 12. |
| `exp_year` | int | metadata | 0 to 65535. |
| `cardholder` | string | metadata | Max 200. |
| `brand` | string | metadata | Optional; detected from the first digit when omitted: `4` visa, `5`/`2` mastercard, `3` amex, `6` discover, else `card`. |

Response `{"credential": {Credential}}`.

#### `PATCH /api/credentials/{id}`

Any of the fields above for the credential's existing kind (`kind` cannot be
changed). All fields are optional and validated as above. Omitted fields
keep their old values:

- Metadata fields that are omitted (or `null`) are kept.
- Supplied secret fields (login: `password`, `notes`; card: `number`, `cvc`)
  are merged into the stored secret, which is decrypted only in memory for
  the merge; omitted secret fields are kept. For example, `{"cvc": "123"}`
  changes only the CVC, and `{"notes": "..."}` keeps the password.

Response `{"credential": {Credential}}`.

#### `DELETE /api/credentials/{id}`

`{"ok": true}`.

### 3.14 Push

See [section 9](#9-push-notifications) for behavior.

#### `GET /api/push/vapid-public-key`

```json
{"enabled": true, "public_key": "<base64url uncompressed P-256 point>", "reason": null}
```

`enabled` is `false` (with a human-readable `reason`) when push is turned
off or has no contact for push services ([section 9](#9-push-notifications));
`public_key` is `null` when push is disabled in config. Pass it as
`applicationServerKey` to `pushManager.subscribe()`.

#### `GET /api/push/subscriptions`

`{"subscriptions": [{PushSubscription}]}` for the caller.

#### `POST /api/push/subscriptions`

Body: the browser's `PushSubscription.toJSON()`. Extra fields such as
`expirationTime` are ignored.

| Field | Type | Notes |
|---|---|---|
| `endpoint` | string | Required, max 2,000. Must be an `https://` URL on a browser push service (`fcm.googleapis.com`, `android.googleapis.com`, `*.push.services.mozilla.com`, `*.push.apple.com`, `*.notify.windows.com`; default port, no credentials), so Iris never sends to addresses on its own network. Iris also follows no redirects when delivering. |
| `keys.p256dh` | string | Required, base64url P-256 public key. |
| `keys.auth` | string | Required, base64url 16-byte auth secret. |

```json
{"endpoint": "https://fcm.googleapis.com/fcm/send/abc", "expirationTime": null,
 "keys": {"p256dh": "B...", "auth": "..."}}
```

Keys are validated by a trial encryption (`400 invalid subscription keys:
...`). Endpoints are unique: re-subscribing an existing endpoint re-binds it
to the calling user and device, replaces the keys, and resets
`failure_count`. Response `{"subscription": {PushSubscription}}`.

#### `DELETE /api/push/subscriptions/{id}`

`{id}` is the subscription id or its URL-encoded endpoint. Response
`{"ok": true}`; `404` if the caller has no such subscription.

#### `POST /api/push/test`

Sends `{"title": "<agent name>", "body": "Notifications are working.",
"kind": "test"}` to all of the caller's subscriptions and waits for the
results:

```json
{"delivered": 1, "failed": 0}
```

Returns `{"delivered": 0, "failed": 0}` when push is disabled.

### 3.15 Jobs

Both routes are **owner-only**. Jobs: `heartbeat` and `feed` (see
[section 5](#scheduled-jobs)).

#### `GET /api/jobs`

```json
{
  "jobs": [
    {"name": "heartbeat", "enabled": false, "interval_mins": 60,
     "active_start": "08:00", "active_end": "22:00",
     "last_run_at": null, "running": false},
    {"name": "feed", "enabled": false, "interval_mins": 360,
     "active_start": "08:00", "active_end": "22:00",
     "last_run_at": 1760000000000, "running": false}
  ]
}
```

`enabled`, `interval_mins`, `active_start`, `active_end` come from config
(values above are the defaults). `last_run_at` is when the last run
**started**.

#### `POST /api/jobs/{name}/run`

`{name}` is `heartbeat` or `feed` (else `404 job not found`). Starts the job
now, even if disabled or outside its active hours. Response **`202`**:
`{"started": true}`, or `false` if it was already running.

### 3.16 Voice

#### `POST /api/voice/speak`

Placeholder. Always `501`:

```json
{"error": {"code": "not_implemented", "message": "voice replies are not implemented yet"}}
```

(Auth is still checked first, so an unauthenticated call gets `401`.)

### 3.17 Engines (install and update)

Owner only. Iris can install and update the official CLIs itself, only from
official sources, into its engines directory (`engines.dir`, default
`<data dir>/engines`), which it searches before `PATH`:

| Engine | Source | Verification |
|---|---|---|
| `claude` | `downloads.claude.ai` release channel (`latest`, then that version's `manifest.json`, then the platform binary), the same steps as the official `install.sh` | SHA-256 and size from the manifest |
| `codex` | npm `@openai/codex` | npm registry integrity; install scripts not run |
| `gemini` | npm `@google/gemini-cli` | same |
| `opencode` | npm `opencode-ai` (its native platform binary) | same |
| `browser` | [Iris's computer](#319-iriss-computer-browser): npm `@playwright/mcp` (Microsoft's Playwright MCP server), then its own `install-browser chromium` from Playwright's official downloads | npm registry integrity for the package; Playwright's installer for Chromium |

The npm engines need Node.js on the server. Iris only ever updates or
removes engines it installed; a CLI the operator installed (for example in
`~/.local/bin`) is reported as `external` and never touched. With
`engines.helper` set, installs run as a separate admin user through that
helper (see `deploy/setup-engines.sh`) and the server itself never downloads
anything.

#### `GET /api/engines`

```json
{
  "engines": [{
    "name": "claude", "label": "Claude Code",
    "source": "downloads.claude.ai (Anthropic release manifest, SHA-256 verified)",
    "installed": true, "managed": true, "external": false,
    "version": "2.1.296 (Claude Code)", "managed_version": "2.1.296",
    "auth": "ok", "requires": null,
    "isolation": "split", "agent_user": "iris-agent", "signed_in_as_agent": true,
    "operation": {"op": "update", "state": "done", "error": null, "version": "2.1.296",
                  "started_at": 1760000000000, "finished_at": 1760000017000}
  }],
  "dir": "/home/user/.local/share/iris/engines",
  "privileged_helper": false,
  "auto_update": true
}
```

`installed`: the CLI was found (anywhere). `managed`: Iris installed it.
`requires`: `"nodejs"` when the engine needs npm and it is missing.
`operation`: the last install/update/uninstall (`state` `running`, `done`,
or `failed` with `error`), or `null`.

Each engine reports `isolation` (`single_user` or `split`), `agent_user`
(the dedicated account or null), and `signed_in_as_agent` (true/false when
its sign-in is known in split mode; null in single-user mode or when unknown).
Version, model and auth probes and app-driven sign-in use the configured
account. Gemini's `mcp` capability is false while project context is disabled
through restricted mode.

#### `POST /api/engines/{name}/install`, `POST /api/engines/{name}/update`

Start in the background; both are idempotent (an up-to-date engine is left
as is). Response `{"started": true | false, "engines": [...]}` (`false` if
an operation for that engine is already running). Progress and results are
broadcast as `engines.changed`. After an install, the harness is re-probed;
if it is not signed in, a "Sign in to <engine>" card is posted. Unknown
engine: `404`.

#### `DELETE /api/engines/{name}`

Removes an engine Iris installed (`400` if Iris did not install it).

#### Automatic updates

With `engines.auto_update` (default `true`), the scheduler updates the
engines Iris installed once a day, when no turn is running (and the browser
only while it is closed). Any install, update, or removal of the browser
closes it first and keeps it from starting until done. Claude Code
updates swap a new verified binary in atomically, so running turns are not
interrupted.

#### First run

When the server starts and no engine can answer, Iris posts one
`engine_setup` [card](#card) in the owner's Main chat listing the engines
with install actions. Installing one is followed by its sign-in card; the
setup card becomes `done` once any engine is signed in.

### 3.18 Safety: pause, disconnect, trusted people

Owner only.

#### `GET /api/safety`

```json
{
  "paused": false, "paused_at": null,
  "trusted_recipients": ["sam@example.com", "+15550102000"],
  "pending_approvals": 0,
  "limits": {"approval_timeout_secs": 300, "max_sends_per_hour": 5,
             "max_calls_per_hour": 200, "max_read_mb_per_hour": 25}
}
```

#### `PUT /api/safety`

`{"paused"?: bool, "trusted_recipients"?: string[]}`. Returns the `GET` body.

- **Pause Iris** (`paused: true`): cancels every running turn (their waiting
  approvals become `cancelled`), including feed model helpers. Feed fetching
  stops after the current bounded phase, without publishing its batch. Until resumed no turn or scheduled job
  starts and no connector call runs. A message sent while paused is stored
  and answered right away with a friendly "I'm paused" reply
  (`meta.error.kind: "paused"`, no push); `POST /api/jobs/{name}/run` and
  `POST /api/feed/refresh` answer `409 paused`. Broadcasts
  `settings.changed` (`settings.paused`).
- **Trusted people** (max 200): messages and emails only to these
  recipients run without approval. Entries are normalized like the gate
  does (the address inside `Name <addr>`, lowercased; phone numbers as
  digits with an optional leading `+`).

#### `POST /api/safety/disconnect-all`

**Disconnect everything**: turns every connector off, stops their programs,
cancels waiting approvals, ends browser access for running turns, and
disconnects and closes the browser. Reopen it
with `POST /api/browser/start` to reconnect browser tools. `{"disconnected": <how many were on>,
"connectors": [{Connector}]}`. Turn them back on one by one.

---

### 3.19 Iris's computer (browser)

Owner only. Iris's own headless browser: Microsoft's Playwright MCP server
(`@playwright/mcp`) running Chromium, whose tools the agent gets (the
[browser tools](#the-browser-tools)) and which the owner can watch and take
over from the app. Iris starts the server on its own side and only wraps
it. One browser serves the owner's personal conversations and jobs; its
profile (cookies, so sign-ins) lives in `<data dir>/browser/profile`. It
starts on first use and closes after `browser.idle_mins` (default 10)
without use. Chrome's own password manager and form autofill are turned
off in that profile.

Where it comes from: the server Iris installed from the app
(`POST /api/browser/install`, `<engines dir>/bin/browser`), else
`playwright-mcp` on `PATH`. Chromium: `browser.command` in the config; else
the build Iris installed at the fixed path `<engines dir>/chromium/app/chrome`
that `deploy/setup-engines.sh` approves for Chrome's sandbox; else
Playwright's own download.

#### `GET /api/browser`

```json
{"browser": {BrowserStatus}}
```

<a id="browserstatus"></a>**BrowserStatus**

| Field | Type | Notes |
|---|---|---|
| `enabled` | bool | `browser.enabled` in the config. |
| `available` | bool | Playwright MCP was found (and the Chromium `browser.command` names, if set). An install from before Playwright MCP is not available until updated. |
| `running` | bool | The browser is open. |
| `paused` | bool | "Pause Iris" is on. |
| `disconnected` | bool | Browser tools are disabled after Disconnect everything; reopening the browser reconnects them. |
| `url`, `title` | string | Current page (empty while closed). |
| `controller` | string | `agent`, or `user` while the owner has taken over. |
| `controller_device_id` | string or null | The device in control. |
| `busy` | bool | The agent is carrying out a browser action right now. |
| `active` | array | `[{"conversation_id", "message_id"}]`: turns that have used the browser and are still running (show the chat peek for these). |
| `watchers` | int | App sockets watching the live view. |
| `streaming` | bool | The live view is taking screenshots (only while someone watches). |
| `sandbox` | bool or null | Chrome's sandbox is on (null while closed). |
| `sandbox_unavailable` | bool | The last start failed only because this system cannot start Chrome's sandbox (for example Ubuntu's AppArmor restriction on unprivileged user namespaces). The app then offers [`POST /api/browser/sandbox`](#post-apibrowsersandbox-owner). |
| `allow_no_sandbox` | bool | The owner allowed running without the sandbox on such a system. |
| `sandbox_fix` | object or null | Only with `sandbox_unavailable` and only where AppArmor restricts unprivileged user namespaces (`kernel.apparmor_restrict_unprivileged_userns = 1`): the one-time root step that lets the sandbox start on this machine, `{"kind", "path", "command"}`. `path` is the real path of the browser Iris would start (null if none). `kind` `"apparmor"`: that browser, everything in its directory, and every ancestor are owned by root (or, with the privilege split, the `iris` engine admin account when it owns the engines directory and is neither Iris's nor the agent's account) and not group/other writable, and `command` writes `/etc/apparmor.d/iris-browser` for exactly that path and loads it with `apparmor_parser -r`. `kind` `"setup"`: the browser could be replaced by an unprivileged user, so no profile is offered for it; `command` runs `deploy/setup-engines.sh` (from the Iris source folder) for the account Iris runs as, after which Iris's own browser is installed from the app into the protected engines directory; with it, `install_browser` (bool) is true when the privilege split is configured and that browser is not installed yet or still sits in its pre-fixed-path download directory (an install moves it), so the app offers [`POST /api/browser/install`](#post-apibrowserinstall-owner), `account_conflict` (bool) is true when Iris or its agent runs as the `iris` engine admin account, which the script refuses (move them to another account first), and `configured_command` (bool) is true when `browser.command` is set, which always wins and has to be cleared (then restart Iris). A managed browser at its old, per-build path is never offered a profile: it is moved instead. Show `command` to copy; never run it from a client. Holds paths, never secrets. Computed by `GET /api/browser` (live `browser.state` events from the browser itself omit it). |
| `error` | string or null | Why the last start failed (for example missing system libraries, with the command that fixes it). |
| `managed`, `managed_version` | bool, string or null | Installed from the app. |
| `requires` | string or null | `"nodejs"` when installing from the app needs Node.js first. |
| `install` | object or null | The last install/update, as `operation` in [`GET /api/engines`](#get-apiengines). |

#### `POST /api/browser/start`, `POST /api/browser/stop`

Open (`503 browser_unavailable` with the reason if it cannot start) or close
the browser. Starting reconnects browser tools after Disconnect everything.
Closing is graceful, so cookies are saved. Both return
`{"browser": {BrowserStatus}}`.

#### `POST /api/browser/takeover` (Owner)

The calling device takes control (the browser opens if needed). While the
owner is in control the agent's browser tools wait (up to about 50 seconds
per call, then tell the agent the user is still using it), and the device
sends input over the WebSocket (`browser.input`), including fills of saved
logins and cards. Control returns to the
agent on `POST /api/browser/handback`, when the browser closes, or after 5
minutes without input. Returns `{"browser": {BrowserStatus}}`.

#### `POST /api/browser/handback` (Owner)

Give control back to the agent.

#### `POST /api/browser/navigate` (Owner)

`{"url": "https://..."}`: open a page while this device has control (for
example a site's sign-in page). Only `http` and `https`; a bare domain gets
`https://`. `400` unless this device has control.

#### `POST /api/browser/sandbox` (Owner)

`{"allow_without": true | false}`. Iris always starts Chrome with its
sandbox; where the sandbox cannot start, the browser stays off until the
owner either fixes the server (`sandbox_fix` in
[BrowserStatus](#browserstatus) has the command; see the self-hosting
guide) or allows
running without it here. The choice is saved. `false` takes it back (and
closes a browser running without the sandbox, including one that was
starting). Returns `{"browser":
{BrowserStatus}}`.

#### `POST /api/browser/install` (Owner)

Install (or update) Iris's browser, like
[`POST /api/engines/browser/install`](#post-apienginesnameinstall-post-apienginesnameupdate):
the official `@playwright/mcp` npm package (pinned to a release the real
browser test passed in CI, moved weekly when a newer one passes; an update
installs the pin) into `<engines dir>/chromium`,
started by `<engines dir>/bin/browser`, then its `install-browser chromium`
into `<engines dir>/chromium/browsers`; the build then moves to the fixed
`<engines dir>/chromium/app` (`app/chrome` on Linux) and the download
directory is removed. An update whose Playwright pins the same Chromium
build downloads only the server. The version reads like
`0.0.83 (Chromium build 1247)`.
Needs Node.js. Returns `{"started": bool, "browser": {BrowserStatus}}`;
progress arrives as `browser.state`. If Chromium then fails to start for
missing system libraries, `error` says so; the fix needs root once on the
server (`npx playwright install-deps chromium`, or your distribution's
Chromium package).

## 4. WebSocket: `GET /api/ws`

A single WebSocket carries all real-time events for the authenticated user.

### Connecting and authenticating

Two options:

1. Send `Authorization: Bearer iris_...` on the upgrade request (native
   clients).
2. Browsers, which cannot set headers on WebSocket requests: connect without
   the header (or with an invalid one) and send this as the **first text
   frame within 10 seconds**:

   ```json
   {"type": "auth", "token": "iris_<64 hex chars>"}
   ```

The HTTP upgrade always succeeds. If authentication fails (bad token, wrong
first frame, non-text frame, or timeout) the server sends one frame and
closes:

```json
{"type": "error", "code": "unauthorized", "message": "authenticate first"}
```

Maximum incoming frame size is 64 KiB.

### Server frames

Connection-level frames (no `at` field):

| Frame | Shape | Notes |
|---|---|---|
| `ready` | `{"type": "ready", "user_id": "...", "device_id": "...", "settings": {Settings}, "harnesses": [{HarnessStatus}]}` | Sent once after auth. The subscription starts before `ready`, so no event is missed between the two. Harness `path` is only filled in for the owner. |
| `pong` | `{"type": "pong"}` | Reply to a client `ping`. |
| `resync` | `{"type": "resync"}` | The client fell behind (server buffer of 512 events) and events were dropped. Refetch state over REST. |
| `error` | `{"type": "error", "code": "unauthorized", "message": "authenticate first" \| "device revoked"}` | Followed by close. |

Every **event** below carries an added `at` field (server time in ms when it
was emitted). Events are delivered only to the users listed under
"Audience".

| Event | Payload | Audience |
|---|---|---|
| `message.created` | `{"conversation_id", "message": {Message}}` | Conversation members |
| `message.delta` | `{"conversation_id", "message_id", "text"}` | Conversation members |
| `message.activity` | `{"conversation_id", "message_id", "activity": {Activity}}` | Conversation members |
| `message.completed` | `{"conversation_id", "message": {Message}}` | Conversation members |
| `turn.status` | `{"conversation_id", "message_id", "status": "queued" \| "running"}` | Conversation members |
| `conversation.created` | `{"conversation": {Conversation}}` | Conversation members |
| `entities.changed` | `{"kinds": ["goals" \| "ideas" \| "feed" \| "library", ...]}` | The affected user |
| `settings.changed` | `{"settings": {Settings}}` | All users |
| `harness.status` | `{"harness": {HarnessStatus}}` (`path` always `null`) | All users |
| `message.updated` | `{"conversation_id", "message": {Message}}` | Conversation members |
| `models.changed` | `{"slots": [{Slot}]}` | All users |
| `harness.login` | `{"login": {LoginStatus}}` | The owner |
| `engines.changed` | `{"engines": [...]}` (as in `GET /api/engines`) | All users |
| `browser.state` | `{"browser": {BrowserStatus}}` (may omit the setup fields `managed`, `managed_version`, `requires`, `install`, `sandbox_fix`; merge into what you have) | The owner |
| `browser.active` | `{"conversation_id", "message_id"}` | Conversation members |
| `browser.done` | `{"conversation_id", "message_id"}` | Conversation members |
| `connectors.changed` | `{}` | The owner |

Example:

```json
{"type": "message.delta", "conversation_id": "c0ffee...", "message_id": "a1b2...", "text": "Sure, ", "at": 1760000000123}
```

Event details:

- **`message.updated`**: a stored message changed after it was created; so
  far only server cards whose `state` moves (sign-in, setup, connector
  reconnect, approvals). Replace the message by id.
- **`connectors.changed`**: a connector was added, changed, signed in,
  paused, resumed, or removed; refetch `GET /api/connectors`.
- **`models.changed`**: a slot's automatic choice changed (a model was
  installed or disappeared, a CLI signed in or out, the codex models cache
  changed; checked every minute) or the owner pinned/unpinned a slot.
- **`message.created`**: a new user message, assistant placeholder, card
  message the agent created during a turn, or agent-initiated message (for
  example a heartbeat post in the Main chat).
- **`message.delta`**: a raw chunk of assistant text, in order. Concatenate
  for live display; the final `content` in `message.completed` is the
  trimmed full text and replaces it.
- **`message.activity`**: tool-use progress. The same `activity.id` is sent
  again as it moves from `started` to `completed` or `failed`; replace by id.
- **`message.completed`**: the assistant message in its final state
  (`complete`, `error`, or `cancelled`) with full `meta`.
- **`turn.status`**: `queued` is sent only when every CLI slot is busy as the
  turn starts waiting; `running` when the turn gets a slot. There is no
  terminal `turn.status`; use `message.completed`.
- **`entities.changed`**: refetch the listed collections. Sent after REST
  changes (to the caller), after a turn in which the agent successfully
  changed data through the bridge (to the conversation creator; see
  [section 8](#8-the-iris-mcp-bridge)), and when a feed pipeline run
  publishes or finishes (`["feed"]`).
- **`conversation.created`**: sent by `POST /api/conversations`,
  `POST /api/feed/{id}/discuss` (only when a new discussion is created),
  `POST /api/ideas/{id}/start`, and to the added user by
  `POST /api/conversations/{id}/members`. Removing a member sends no event.

- **`browser.active`** / **`browser.done`**: the turn answering
  `message_id` used Iris's browser for the first time / ended. Show the
  browser peek under that streaming message in between.
- **`browser.state`**: Iris's browser opened or closed, changed page or
  title, changed hands (take over / hand back), started or finished an
  action, or its setup state changed.

Scheduled jobs run in hidden `job` conversations, and their turns emit no
`turn.status`, `message.*`, or card events at all. A job only becomes
visible through its results: `message.created` for a heartbeat post in the
Main chat, and `entities.changed` for data it changed.

### Client frames

| Frame | Effect |
|---|---|
| `{"type": "auth", "token": "..."}` | Only meaningful as the first frame. |
| `{"type": "ping"}` | Server replies `{"type": "pong"}`. |
| `{"type": "browser.watch", "mode": "peek" \| "full" \| "off", "width": 1000}` | Owner only. Start (or change, or stop) the live view of Iris's browser on this socket. `width` (optional): how many device pixels wide the app shows the picture (its CSS width times `devicePixelRatio`). |
| `{"type": "browser.input", "kind": ...}` | Owner only, while this device has [taken over](#post-apibrowsertakeover-owner). See below. |

Any other client frame is ignored. Non-owners get `{"type": "error", "code":
"forbidden", ...}` for the two browser frames (the socket stays open).

**Live view.** While watching, the socket receives connection-level frames
(no `at`):

```json
{"type": "browser.frame", "seq": 41, "mode": "full", "width": 500, "height": 1000, "data": "<base64 JPEG>"}
```

`width` and `height` are the page viewport in CSS pixels. The JPEG itself is
as wide as the widest watcher asked (between 120 and 1000 px; without
`width`, 1000 for `full` and 500 for `peek`), at JPEG quality 55 (`full`)
or 50 (`peek`), captured by Chrome directly through the browser server
(nothing is written to disk). On typical pages that is about 9 to 16 KB
for the chat thumbnail and 90 to 220 KB for full screen, down from 70 to
170 KB and 170 to 450 KB. Iris takes one about every 0.3 seconds (`full`)
or every second (`peek`) and sends a frame only when the picture changed;
throttling never drops the newest frame, so a still page always ends up on
screen. While the agent waits for a slow page, screenshots wait too. A new
watcher gets the current picture at once. Screenshots are taken only while
some app is watching. Stop watching (`"off"`)
when the view is hidden or the app goes to the background.

**Input while in control.** Coordinates are fractions (0 to 1) of the frame
image:

| `kind` | Fields | Effect |
|---|---|---|
| `tap` | `x`, `y` | A left click at that point. A window the page opens while the owner is in control (a sign-in popup) becomes the current tab. Taking over keeps the tab the agent was on (the agent selects a sign-in window before asking). |
| `scroll` | `x`, `y` (optional), `dx`, `dy` | Mouse wheel; deltas in CSS pixels (max 4,000). |
| `text` | `text` (max 2,000 characters) | Typed into the focused element. |
| `key` | `key` | `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, arrows, `Home`, `End`, `PageUp`, `PageDown`. |
| `back` | none | History back. |
| `navigate` | `url` | Open an http(s) page (as `POST /api/browser/navigate`). |
| `fill` | `credential_id`, `field` | Fill one field of a saved [credential](#313-credentials) into the focused element (its contents are replaced; the element is checked and filled in one step, in its own frame): `username` or `password` for a login, `number`, `exp` (MM/YY), `exp_month`, `exp_year` (a list's own option, or what the field takes), `cvc`, or `name` for a card. A login only on the site it was saved for; everything only over HTTPS (plain HTTP only on this machine). The value never comes back to any client. |

Typing into a private field (a password, card, or one-time-code field, by
its type, autocomplete, name, or label, or a field in another site's
frame; checked before the first `text` input, again after any other input, and at least every 3 seconds of typing), and filling a `password`,
`number`, `exp`, or `cvc`, closes that page to the agent before anything is
typed: its browser tools other than `browser_navigate`,
`browser_navigate_back`, `browser_tabs`, `browser_handle_dialog`, and the
fill tools answer that the owner typed private details there. Both the document (in whichever tab, also after its
address changes within it) and its address (ignoring the fragment; a page
brought back from history may still hold what was typed) stay closed until
the browser closes. Pages and addresses are read from the browser itself,
never from page-controlled text. A login is filled only when its site
matches that real address and the focused field is not in another site's
frame.

While the owner is in control, a dialog the page opens is answered for
them (alerts and confirmations accepted, prompts dismissed) and a file
picker is cancelled; the input is then carried out.

Input from a device that does not have control, or a failed input, gets
`{"type": "browser.input_error", "message": "..."}`. Input is carried out in
order on its own task, so a slow page never holds up the socket's frames or
events (at most 64 inputs wait; more are refused with that error). Each
input step re-checks that this device still has control; input still
queued when the socket closes is dropped, and a revoked device loses
control at once. Typed text and filled values are never logged.

### Keepalive and revocation

The server sends a WebSocket Ping control frame every 30 seconds. Every
second ping (about once a minute) it checks whether the device was revoked;
if so it sends `{"type": "error", "code": "unauthorized", "message":
"device revoked"}` and closes.

An open WebSocket also marks the user as online, which suppresses reply push
notifications (see [section 9](#9-push-notifications)).

---

## 5. Turn lifecycle

A **turn** is one assistant reply produced by a harness CLI (`claude`,
`codex`, `gemini`, or `opencode`). Iris only orchestrates: the CLI does all reasoning and tool use.

### Chat turns

After `POST /api/conversations/{id}/messages` (or `POST
/api/ideas/{id}/start`):

1. Iris stores the user message (`role: "user"`, `status: "complete"`) and an
   assistant placeholder (`role: "assistant"`, `content: ""`,
   `status: "streaming"`), emits `message.created` for each, and returns
   `202`.
2. The turn waits for the conversation (one turn at a time per conversation)
   and then for a global CLI slot (`max_concurrent_turns`, default 2). If all
   slots are busy it emits `turn.status` `queued`. When it gets a slot it
   emits `turn.status` `running`.
3. It classifies the message (effort, and whether it is clearly hard), picks
   a slot and model ([section 7](#7-engine-selection)), and records it on the
   placeholder (`harness`, `model`, `effort`, and `meta.routing`; no event is
   sent for this). Images are left out with a conversational note when the
   chosen model cannot see them, and the bridge is skipped for a CLI without
   MCP support.
4. It resumes the CLI session for this conversation and harness when possible,
   replaying messages the session has not seen (up to 40 messages / 24,000
   characters), wires in the [iris-mcp bridge](#8-the-iris-mcp-bridge) and
   enabled connectors, and starts the CLI. In shared conversations (more than
   one member) the bridge runs in cards-only mode; connectors and the
   owner's private files are only passed in the owner's own conversations
   (see [section 3.3](#33-conversations-and-messages)). Connectors reach the
   CLI only as gateway proxies ([3.12](#312-connectors)); a connector call
   that needs approval posts its `approval` card (`message.created`) and
   pushes right away, while the turn waits.
5. While the CLI runs, Iris emits `message.delta` and `message.activity`.
6. On a "not signed in" error with no text yet, the harness is marked
   `auth_required` (a `harness.status` event, plus a reconnect card if the
   owner had used it) and the turn is retried on the next-ranked engine. On a failed session resume with no text yet, the turn is
   retried with a fresh session. At most 3 attempts in total.
7. Iris saves the final message and emits `message.completed`, then
   `message.created` for each card the agent created during the turn
   through the bridge (server cards such as approvals were sent as they
   were posted; a `connector_reconnect` card is posted if a connector
   refused its sign-in),
   then `entities.changed` if applicable, then sends a reply push if
   applicable.

The text streamed through `message.delta` is not saved until the turn ends.
All events above go to the conversation's members, except `entities.changed`
(to the conversation creator). Turns in hidden job conversations emit none
of the conversation events (see [Scheduled jobs](#scheduled-jobs)).

### Assistant message statuses

| Status | Meaning |
|---|---|
| `streaming` | Turn in progress (placeholder). |
| `complete` | Finished normally. |
| `error` | Failed. `meta.error` is set; `content` is the partial reply if any text streamed, otherwise a friendly first-person explanation. |
| `cancelled` | Cancelled via `POST /cancel` (or conversation deletion, or shutdown). `content` is whatever text streamed (possibly empty). |
| `interrupted` | The daemon stopped while the turn was running; set at the next startup. `content` is empty because streamed text is only saved at the end. No event is sent. |

User messages and other stored messages always have status `complete`.

### The `meta` object

On a finished assistant message (`message.completed`):

```json
{
  "routing": {
    "router": "automatic",
    "model": "claude:haiku",
    "effort": "medium",
    "slot": "chat",
    "escalated": false,
    "score": 3,
    "signals": ["code"],
    "reason": "code",
    "notes": []
  },
  "purpose": "chat",
  "activity": [
    {"id": "toolu_01", "kind": "mcp", "name": "iris/create_goal", "status": "completed", "summary": ""}
  ],
  "usage": {"input_tokens": 3, "output_tokens": 20, "cache_read_input_tokens": 100,
            "cache_creation_input_tokens": 0, "duration_ms": 2100},
  "duration_ms": 2450
}
```

| Field | Type | Notes |
|---|---|---|
| `routing` | object or null | The engine decision ([RouteDecision](#routedecision)) plus `notes`, a list of strings describing retries and reroutes (e.g. `"claude:haiku signed out; rerouted"`, `"resume failed: ..."`, `"persona changed; started a new session"`, `"opencode:ollama/qwen3:8b cannot see images; image left out"`). `null` if no engine could be chosen. |
| `purpose` | string | `"chat"` or `"scheduled"`. |
| `activity` | [Activity](#activity)[] | Final state of every tool-use activity in the last attempt. |
| `usage` | object | Present only if the CLI reported usage. Passed through per harness. Claude: `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`, `duration_ms`. Codex: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`. Gemini: `input_tokens`, `output_tokens`, `cached_tokens`, `tool_calls`, `duration_ms`. OpenCode: `input_tokens`, `output_tokens`, `reasoning_tokens`, `cache_read_tokens`, `cache_write_tokens`, `cost`, `steps`. |
| `duration_ms` | int | Time from placeholder creation to completion, including any queueing. |
| `error` | object | Present only when `status` is `error`: `{"kind": "...", "detail": "..."}`, `detail` at most 500 characters. |

While a turn is running, the placeholder's `meta` is `{}` and then
`{"routing": {...}}` once an engine is chosen.

A Main chat message posted by the heartbeat job has
`meta: {"initiated_by": "heartbeat", "routing": {...}}`.

### Error kinds

| `meta.error.kind` | Cause | `detail` | Friendly `content` (when no text streamed) |
|---|---|---|---|
| `auth_required` | A harness reported during the turn that it is not signed in (after text had streamed, or with no other engine left), or no engine was usable and at least one enabled harness **allowed in this conversation** is signed out | The signed-out harness name(s), comma-separated, e.g. `"claude"` or `"claude,codex"` | "I can't think right now because my AI engine on the server is signed out. To fix it, run \`claude\` on the Iris server and sign in with /login, then send your message again." (the fix names each listed harness: Codex "run \`codex login\`", Gemini "run \`gemini\` ... and sign in", OpenCode "run \`opencode auth login\`"; the app's reconnect card is the easier fix) |
| `paused_policy` | A conversation with more than one person, Claude is the only usable engine, and the owner has not allowed Claude for shared chats | `only Claude is available and this conversation is shared` | "I can't reply in this group chat yet. The only AI engine on the server is Claude, and Claude subscriptions are for one person, so I stay out of shared conversations unless the owner turns that on." The first time per conversation an `action` card offers the owner the opt-in. |
| `no_engine` | No usable engine at all (none installed, all disabled, or every harness failed to start) | `no usable engine` / `no engine available` | "I can't reach any AI engine on the server right now. One of Claude, Codex, Gemini, or OpenCode needs to be installed and signed in; I can walk you through it in Settings." |
| `timeout` | No result within `turn_timeout_secs` (default 600) | `no result after 600s` | "That took too long, so I stopped. Want me to try again?" |
| `paused` | The owner paused Iris | `the owner paused Iris` | "I'm paused right now, so I didn't do anything. You can resume me from my computer screen in the app whenever you're ready." (no "stuck" push) |
| `harness_error` | The CLI reported an error, or exited without a result | The CLI's error text | "Something went wrong on my end. Want me to try again?" |
| `internal` | Iris could not build the CLI command | `could not build command: ...` | "Something went wrong on my end. Want me to try again?" |
| `paused` | The owner turned on "Pause Iris" (the engine is not run; no "stuck" push) | `the owner paused Iris` | "I'm paused right now, so I'm not doing anything. You can turn me back on in my computer settings whenever you want me again." |

The friendly text is written to be shown as the agent's own reply; clients do
not need special error UI, though they may style `status: "error"`
differently.

### Engine details are for operators

`harness`, `model`, `effort`, and `meta.routing` / `meta.usage` record which
engine produced a reply and why. They are meant for debugging and the
optional advanced "AI engine" screen; **end-user UIs should not show them**.
`model` on a message is the catalog id (e.g. `"claude:haiku"`), not the raw
CLI model name.

### Cancellation and concurrency

- One turn runs per conversation; later messages queue behind it.
- `POST /cancel` stops the active turn: the CLI process group gets SIGTERM.
  The session id is kept so the conversation can continue.
- Deleting a conversation cancels its turn first. Daemon shutdown cancels all
  running turns.

### Scheduled jobs

The `heartbeat` job runs as a turn with `purpose: "scheduled"` in a hidden
job conversation owned by the owner, on the `background` slot (low effort);
the `feed` job is the For you pipeline (below). Both are the owner's own
use, so any signed-in engine may run them, Claude included. Each heartbeat
run starts a fresh CLI session (no resume, no replay), and each job
conversation is trimmed to its last 20 messages before every run. Job turns
are not streamed: no `turn.status`, `message.*`, or card events are sent for
them. The scheduler
checks every minute and runs a job when it is enabled, the local time is
inside `[active_start, active_end)` (windows may wrap past midnight; equal
start and end means all day), and at least `interval_mins` have passed since
the last start. Manual runs (`POST /api/jobs/{name}/run`,
`POST /api/feed/refresh`) skip those checks.

- **heartbeat**: the agent works through `HEARTBEAT.md`. If it replies
  `HEARTBEAT_OK` (or the reply contains it, or is empty), nothing happens.
  Otherwise the reply is posted as a new assistant message in the owner's Main
  chat (`message.created`, `meta.initiated_by: "heartbeat"`) and pushed to all
  of the owner's subscriptions.
- **feed**: the For you pipeline for the owner. It publishes at most one
  batch per local day; scheduled runs after that (every `interval_mins`,
  default 60, between `active_start` 06:00 and `active_end` 22:00) do
  nothing. A run that publishes nothing (for example during a network
  outage) leaves the day open, so the next check tries again; the last-run
  time is recorded even when a run fails, so that happens after
  `interval_mins`, not every minute.

Failed job turns are only logged.

### For you pipeline

Deterministic code does nearly everything. The model is used at most twice
a day, counting manual refreshes and failed calls: once for interests and
once for summaries (later runs that day use the last interests and the
posts' own descriptions). Each call goes through the owner's own CLI as a
single prompt with no session, no Iris persona, and tools turned off with
the CLI's own controls: Claude Code `--tools "" --strict-mcp-config`;
Codex `--ignore-user-config --ignore-rules --ephemeral`, every
tool-providing feature disabled (`shell_tool`, `unified_exec`, `apps`,
`plugins`, browser and computer use, `view_image`, `image_generation`,
`code_mode_host`, ...), `sandbox_mode="read-only"`,
`approval_policy="never"`, and `web_search="disabled"`. The engine's `extra_args` from the config are
left out of these calls (they could add tools back). Gemini CLI and
OpenCode cannot reliably override tools, MCP servers, or agent permissions
from the user's own settings, so these calls skip them (if only they are
signed in, the feed uses the posts' own text). The day's call is reserved
before it starts, so a restart mid-call cannot cause a second one; it is
given back only when no model ran. Each call is one real model run: a
signed-out CLI falls through to the next engine, a failure does not.
The activity log shows each run, in the hidden `feed` job conversation,
with the engine that wrote the text.

1. **Interests** (once a day, `background` slot): `USER.md`,
   `PROACTIVE_PREFERENCES.md`, the feed prompt, recently liked posts, the
   owner's own recent chat messages (main and side chats, last 14 days,
   never shared chats), and liked YouTube videos (when Google is connected)
   become 3 to 8 topics with keywords and search queries, the user's home
   city and language, and up to 8 source suggestions (added as
   `suggested` sources; at most 40 kept). Suggestions on fixed hosts
   (subreddits, YouTube channels, Hacker News) start on; feeds on other
   sites, Bluesky handles, and Mastodon accounts start off until the owner
   turns them on, because the model saw private notes and untrusted titles
   and must not be able to make Iris contact a host of its choosing. Without a usable reply the last
   interests are kept, or topics are derived from liked posts.
2. **Fetch**: enabled sources (at most 40 per run; at most 4 subreddits,
   least recently read first), plus one Hacker News search per topic and
   one Mastodon hashtag per topic. YouTube channel feeds come from the
   user's sources and, when a Google token is available, their
   subscriptions (`origin: google`).
3. **Clean**: posts older than 7 days, recurring "weekly thread" posts,
   posts in another language (when the source says), and anything already
   shown (same canonical link or title as any post still in the feed, or
   recorded for 120 days) are dropped; at most 40 items per source and 1,500
   overall go on; near-duplicate titles across sources are merged.
4. **Rank**: likes-weighted topic affinity (a topic's stated weight, raised
   by likes and discussions over 60 days, lowered when its posts are
   ignored), keyword match, recency (36-hour half-life), the source's own
   popularity numbers, photos, and liked sources and words. Topic-search
   results score a little below followed sources. The best 3x the daily
   size are enriched from their pages (OpenGraph, Twitter cards, JSON-LD:
   title, description, site name, icon, original photos, price, event;
   every finalist except posts that live on Bluesky, Mastodon, Reddit,
   Hacker News, or YouTube), dropped if the page shows they are older
   than 7 days;
   duplicates found only now (a link that redirects to, or names as
   canonical, a page already shown or picked) are dropped; then the day's
   posts are picked: each competitive topic's best post
   first, then by score with diminishing returns for repeats, at most 3 per
   source, 5 per topic (story groups), and about a third from searches.
5. **Blurbs** (one batched call, `feed` slot): posts whose own description
   is 110 to 600 characters, complete, and not link soup keep it; the rest
   get a model-written `hook`, summary (`body`), and `why`. Without a model
   the description (or title) is used and `why` is rule-based ("Because
   you're into ...").
6. **Publish**: up to 3 photos and the icon per post are downloaded and
   stored locally (`<data dir>/feed-images`, images under 200 px wide are
   skipped). Only these checked copies are shown: a photo Iris could not
   fetch (robots.txt, a non-public address, an error) is left out rather
   than loaded by the phone from its original URL. Posts, seen keys, and
   the batch bookkeeping are written in one transaction, with unique
   timestamps (the best post newest).

Both model calls use the turn pipeline's slot routing and isolation checks:
no owner connectors, CLI memory, or tools. Engines that cannot guarantee
a tool-free isolated call are skipped. Pause Iris and shutdown cancel
running calls.

**Retention.** Unliked, undiscussed pipeline posts are removed after 90
days with their photos; posts the agent wrote are kept.

**Adaptive size.** Default 10 a day, between 5 and 30. Once a day, over
the last 7 days with a batch (at least 3): if the feed was opened on 3 or
more and the user reached the end on at least 60% of those, the size grows
by a quarter (at least 2); if it was opened on at most 2 of 7, it shrinks
by a fifth (at least 2).

**Pause.** When the feed has been published for 14 days and not opened (or
resumed) in 14 days, the daily run pauses it instead and posts one message
in the Main chat with an `action` card (`POST /api/feed/resume`). No push.

**Urgent pushes** (`kind: "feed_urgent"`, at most 2 a day, delayed by
quiet hours): a deterministic score of at least 70. Price drops on saved
products (re-checked daily, up to 10 a run): 50 + 2 x percent off, for drops
of at least 5% and 1 unit in the same currency, so 10% off pushes; the drop
also becomes a `price_drop` post. Events from today's posts: 25, +15 within
7 days (+8 within 14), +15 for a core topic (+8 for a moderate one), +25
when the event's location names the user's city, +5 from a followed source,
for events in the next 30 days. Without the city match an event cannot
reach 70.

**Polite fetching.** `User-Agent: IrisFeed/<version> (self-hosted personal feed
reader)`. robots.txt (redirects followed) is honored for every page read for a
preview, every photo downloaded, and a website address (not a feed
address like `/feed` or `.xml`) until it has proven to be a feed (finding a
site's feed is crawling); `5xx` or unreachable
robots.txt means "not this run". Confirmed feeds and the documented
platform feeds and APIs (YouTube channel feeds, Reddit, Hacker News,
Bluesky, Mastodon, the YouTube Data API) are read like a feed reader reads
them, once a day (in full, so items not picked today are still candidates
tomorrow). One request per
host per second (more
with `Crawl-delay`, up to 10 s; rate-limit headers such as Reddit's
`x-ratelimit-reset` and `Retry-After` are honored, and a host that asks for
a pause longer than 75 s is left alone for the rest of the run); 8 s connect and 20 s
overall timeouts; size caps of 4 MB for feeds and JSON, 1.5 MB for pages
(truncated), and 6 MB for photos. Only http(s) to public addresses:
no ambient proxy; hostnames that resolve to loopback, private, link-local, or carrier-grade
NAT (tailnet) addresses are refused on every redirect hop.

**Google (YouTube).** The run uses the owner's active Google connector's
vault-encrypted `OAuthConfig`, refreshing its access token through the same
OAuth path as the connector gateway. Google sign-in and reconnect request
`https://www.googleapis.com/auth/youtube.readonly`. The run reads
`subscriptions.list` and `videos.list?myRating=like` from the YouTube Data API
(all subscriptions, up to 250, become `google` sources read in rotation).
Older connections that return insufficient-scope errors get one Main chat
note asking the owner to reconnect Google to add YouTube access; the rest of
the feed continues. Without a usable Google connection, YouTube works through
channel sources. Tokens are never exposed to the CLI, API, or logs.

---

## 6. Data model

Fields marked "(not in JSON)" exist in the database but are never
serialized.

### User

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `display_name` | string | The initial owner is named `"Owner"`. |
| `is_owner` | bool | |
| `created_at` | int | |

### Device

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `user_id` | string | |
| `name` | string | Max 80. |
| `created_at` | int | |
| `last_seen_at` | int or null | Updated at most once a minute. |
| `revoked_at` | int or null | |

The token hash is never serialized.

### Conversation

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `kind` | string | `main`, `side`, or `group` (`job` is never returned). |
| `title` | string | Main chat: `"Main chat"`. |
| `seed` | object or null | What started a side chat: `{"type": "feed_post", "id": "..."}` or `{"type": "idea", "id": "..."}`. |
| `created_at` | int | |
| `updated_at` | int | Bumped by every new message and by renames. |
| `archived_at` | int or null | |
| `member_count` | int | Number of human members. More than 1 means the conversation is shared. |

The conversation's creator is not included; use the members list (role
`owner`).

### Member

Returned by `GET /api/conversations/{id}/members`.

| Field | Type | Notes |
|---|---|---|
| `user_id` | string | |
| `display_name` | string | |
| `role` | string | `owner` (the conversation's creator) or `member`. |
| `joined_at` | int | |

### Message

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `conversation_id` | string | |
| `role` | string | `user`, `assistant`, or `system`. |
| `author_user_id` | string or null | Set for user messages. |
| `kind` | string | `text`, `card`, or `system_note` (short service-generated outcome; render as a subtle centered line). |
| `content` | string | Text (Markdown from the agent). For cards, a plain-text fallback (the card title, or a summary for feed-post cards). |
| `attachments` | string[] | Attachment ids. |
| `card` | object or null | See [Card](#card). |
| `status` | string | `complete`, `streaming`, `error`, `cancelled`, `interrupted`. |
| `harness` | string or null | Assistant messages: `claude`, `codex`, `gemini`, or `opencode`. Hide from end users. |
| `model` | string or null | Catalog id, e.g. `claude:haiku`. Hide from end users. |
| `effort` | string or null | Reasoning effort, or null for the CLI default. Hide from end users. |
| `meta` | object | See [the meta object](#the-meta-object). `{}` for most non-assistant messages. |
| `created_at` | int | Strictly increasing within a conversation. |
| `updated_at` | int | |

### Card

Cards are structured UI elements with `kind: "card"`. Agent-created cards
(`create_card` bridge tool) have `role: "assistant"`; the feed-post card at
the top of a Discuss chat has `role: "system"`.

| Field | Type | Notes |
|---|---|---|
| `type` | string | Agent cards: one of `link`, `connect` (with `service` set to a catalog id such as `google`, `github`, `notion`, or `custom` it opens Iris's own setup and needs no `url`), `info`, `feed_post`, `idea`, `goal`, `result` (one concrete result such as a place or product: image, details, one action). Server cards (below): `action`, `engine_signin`, `engine_setup`, `connector_reconnect`, `approval`. |
| `title` | string | Required, non-blank, max 300. |
| `body` | string | Optional, max 4,000. |
| `url` | string | Optional; must be `http://` or `https://`, max 2,000. Required for `link` and `connect`. |
| `image_url` | string | Optional image (`result` cards, or a `link` card shown as a result); http(s), max 2,000. |
| `action_label` | string | Optional button label, max 60. |
| `service` | string | Optional service name for `connect` cards (e.g. `google`), max 60. |
| `ref_id` | string | Optional id of a related entity. |

Optional fields are omitted when empty, and unknown fields are dropped. The
Discuss feed-post card is built by the server instead: it always includes
`body` (the post body cut to 4,000 characters, plus `…` when cut), `url`
(possibly `null`), and `ref_id` (the post id).

#### Server cards

Iris itself posts these (the agent cannot create them). All but `approval`
carry `owner_only: true`: only the owner can act on them. Cards whose `state`
changes are re-sent as `message.updated`.

| `type` | Extra fields | Action |
|---|---|---|
| `action` | `action: {"method", "path", "body"}` | Call that Iris API route with that JSON body when the owner taps `action_label` (e.g. `PUT /api/settings {"allow_claude_for_shared": true}`). |
| `engine_signin` | `harness`, `flow` (`code_paste`, `device_code`, `manual`), `state` (`needs_reconnect`, `starting`, `waiting_for_code`, `waiting_for_approval`, `verifying`, `done`, `failed`, `cancelled`), `url`, `user_code`, `error`, `instructions` (manual flow), `login_id` | Start with `POST /api/harnesses/{harness}/login`. For `waiting_for_code`, show `url` and a code field (`/login/code`); for `waiting_for_approval`, show `url` and `user_code`; for `manual`, show `instructions` and a "Check again" button (the same `POST`). |
| `engine_setup` | `state` (`needs_setup`, `done`), `options: [{"harness", "label", "installed", "requires", "action": {"method", "path"}}]` | Each option installs that engine (`POST /api/engines/{name}/install`). |
| `connector_reconnect` | `connector_id`, `service`, `auth` (`oauth` or `secrets`), `state` (`needs_reconnect`, `done`) | `oauth`: sign in again (`POST /api/connectors/{id}/oauth/start`). `secrets`: open the connector's settings so the owner can update its credentials (`PATCH /api/connectors/{id}`). The card turns `done` (✓) after a successful connection. |
| `approval` | `service`, `preview: {"to", "subject", "body", "fields": [{"label", "value"}]}`, `details` (the complete arguments, pretty JSON; requests too large to show in full are refused), `reason` (why it asks, e.g. "Sends a message to someone new: sam@example.com"), `tool`, `category`, `connector_id`, `turn_message_id` (the reply of the turn that asked; absent for scheduled jobs), `action_label` (`Send`, `Pay`, `Delete`, or `Allow`), `approver_user_id`, `owner_only: false`, `state` (`pending`, `approved`, `denied`, `expired`, `cancelled`), `expires_at`, `decided_at` | Show the preview exactly as given with Deny / `action_label`; decide with [`POST /api/messages/{id}/decision`](#post-apimessagesiddecision). Built by the gateway from the real arguments of the held call, never from agent text; characters that could hide or reorder text are shown as `[U+XXXX]`, and a body over 4,000 characters is cut with a note (the full text is in `details`). |


### Attachment

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `mime` | string | `image/png`, `image/jpeg`, `image/gif`, or `image/webp`. |
| `size` | int | Bytes. |
| `sha256` | string | Hex digest of the content. |
| `created_at` | int | |
| `file_name`, `user_id` | | (not in JSON) |

### FeedPost

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `title` | string | Max 300. |
| `body` | string | Markdown, max 50,000. |
| `topic` | string or null | Max 100. |
| `source_url` | string or null | http(s). |
| `source_name` | string or null | Display name of the source (site or creator), max 100. |
| `image_url` | string or null | Cover photo: the first of `images`. |
| `images` | string[] | The source page's original photos, in order (http(s), at most 10). |
| `photos` | string[] | What to display for `images`, in order: `/api/feed/images/<file>` for photos Iris cached; for posts the agent wrote, uncached photos keep their original URL. Pipeline posts list cached copies only. |
| `why` | string or null | One line on why the user would like this post, max 300. |
| `hook` | string or null | A short line for the story viewer (pipeline posts the model summarized). |
| `favicon_url` | string or null | The source's icon (original URL). |
| `favicon` | string or null | What to display for the icon (cached copy when available). |
| `source_kind` | string or null | `rss`, `youtube`, `reddit`, `hn`, `bluesky`, `mastodon`, `price_drop`, or null for posts the agent wrote. |
| `facts` | object | Read from the page: `{"price": {"amount", "currency"}}`, `{"event": {"start_ms", "location"}}`, price drops add `was`; else `{}`. |
| `liked` | bool | |
| `discuss_conversation_id` | string or null | Set by `POST /discuss`. |
| `created_at`, `updated_at` | int | |

### Idea

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `title` | string | Max 300. |
| `description` | string | Max 2,000. |
| `prompt` | string or null | Message sent when the idea is started, max 4,000. |
| `status` | string | `open`, `started`, `done`, `dismissed`. New ideas are `open`. |
| `conversation_id` | string or null | Chat opened by `POST /start`. |
| `created_at`, `updated_at` | int | |

### Goal

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `title` | string | Max 300. |
| `status_text` | string | Free-text progress note the agent keeps current, max 2,000. |
| `suggested` | bool | Proposed by the agent, not yet confirmed by the user. |
| `done` | bool | |
| `done_at` | int or null | |
| `created_at`, `updated_at` | int | |

### LibraryItem

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `kind` | string | `artifact` or `media`. |
| `title` | string | Max 300. |
| `description` | string | Max 2,000. |
| `mime` | string | |
| `size` | int | Bytes. |
| `conversation_id` | string or null | Conversation the agent saved it from. |
| `created_at`, `updated_at` | int | |
| `file_name` | | (not in JSON) |

### Connector

The API view (`public_json`); secret values are never included.

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `name` | string | MCP server name and tool prefix. |
| `description` | string | |
| `transport` | string | `stdio` or `http`. |
| `service` | string | Catalog entry: `google`, `github`, a one-tap service (`notion`, `todoist`, `linear`, `sentry`, `stripe`, ...; see [the catalog](#post-apiconnectorscatalogservice)), or `custom`. |
| `enabled` | bool | |
| `command` | string or null | Program (stdio, or an http server Iris starts). |
| `args` | string[] | |
| `url` | string or null | http. |
| `env_keys` | string[] | Names of stored env variables (values hidden). |
| `header_keys` | string[] | Names of stored headers (values hidden). |
| `auth` | string | `oauth` or `secrets`. |
| `oauth` | object or null | `{"mode", "client_configured", "client_source", "connected", "connected_at", "account", "scopes", "redirect_uri", "needs_more_access"}`; `account` is the signed-in email when the service reports one. `needs_more_access` is true for a signed-in Google connection whose grant predates scopes Iris now requests (it keeps working; signing in again adds them). Never tokens or secrets. |
| `auth_state` | string | `ok`, `needs_sign_in`, or `needs_reconnect`. |
| `policy` | object | `{"mode", "tools"?, "off"?, "version"}` (see `PUT /api/connectors/{id}/policy`): `tools` maps a tool name or Google action key to `allow`, `ask`, or `block`; `off` lists Google services turned off; `version` is the rules format (2). |
| `rules` | string | The mode in plain words, e.g. "Reads freely · asks before sending, buying, or deleting". |
| `paused`, `paused_reason`, `paused_at` | bool, string or null, int or null | Set by a tripwire; cleared by `resume`. |
| `created_at`, `updated_at` | int | |

```json
{"id": "...", "name": "calendar", "description": "", "transport": "stdio", "service": "custom",
 "enabled": true, "command": "npx", "args": ["-y", "some-calendar-mcp"], "url": null,
 "env_keys": ["API_TOKEN"], "header_keys": [], "auth": "secrets", "oauth": null,
 "auth_state": "ok", "policy": {"mode": "standard"},
 "rules": "Reads freely · asks before sending, buying, or deleting",
 "paused": false, "paused_reason": null, "paused_at": null,
 "created_at": 1760000000000, "updated_at": 1760000000000}
```

### ConnectorCall

One call through the gateway (the connector activity log).

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `connector_id`, `connector_name` | string | |
| `tool` | string | Tool name. |
| `category` | string | `read`, `draft`, `write`, `send`, `purchase`, `delete`, `export`, or `unclear`. |
| `decision` | string | `auto` (rules let it run), `trusted` (only trusted recipients), `approved`, `denied`, `expired`, `cancelled`, `blocked` (rules or a tripwire refused it), or `pending`. |
| `outcome` | string or null | `ok` or `error` once it ran; null if it did not run. |
| `reason` | string | Why it asked or was refused. |
| `summary` | string | Short preview: recipients and subject (never the body). |
| `conversation_id`, `message_id` | string or null | The turn that made the call. |
| `card_message_id` | string or null | Its approval card. |
| `result_bytes` | int | Size of the result. |
| `created_at`, `finished_at` | int, int or null | |

### Credential

Metadata only; the encrypted secret is never returned.

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `kind` | string | `login` or `card`. |
| `label` | string | |
| `metadata` | object | login: `{"site", "username", "allow_subdomains"}`; card: `{"brand", "last4", "exp_month", "exp_year", "cardholder"}` (values may be `null`). |
| `created_at`, `updated_at` | int | |
| `last_used_at` | int or null | When Iris last filled it into a page. |

### PushSubscription

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `user_id` | string | |
| `device_id` | string or null | Device that subscribed. |
| `endpoint` | string | |
| `created_at` | int | |
| `last_success_at` | int or null | |
| `failure_count` | int | Consecutive failures. |
| `p256dh`, `auth` | | (not in JSON) |

### HarnessStatus

| Field | Type | Notes |
|---|---|---|
| `name` | string | `claude`, `codex`, `gemini`, or `opencode`. |
| `enabled` | bool | From config. |
| `installed` | bool | The executable was found. |
| `path` | string or null | Resolved executable path on the server. Only shown to the owner (`GET /api/harnesses`, `POST /api/harnesses/refresh`, and the owner's `ready` frame); always `null` for other users and in `harness.status` broadcasts. |
| `version` | string or null | First line of `<cli> --version` (max 80 chars). |
| `auth` | string | `ok`, `auth_required`, or `unknown` (Gemini CLI has no status command: `unknown` until a turn shows otherwise; `unknown` counts as usable). |
| `auth_message` | string or null | Short reason when `auth_required`; never contains account data. |
| `checked_at` | int or null | Last probe. |
| `isolation` | string | `single_user` or `split`. |
| `agent_user` | string or null | Dedicated OS account in split mode; `auth` refers to that account's CLI sign-in. |

### ModelEntry

| Field | Type | Notes |
|---|---|---|
| `id` | string | Stable catalog id `<harness>:<model>`, e.g. `claude:haiku`, `codex:gpt-6-luna`, `gemini:flash`, `opencode:ollama/qwen3:8b`. |
| `harness` | string | |
| `model` | string | Value passed to the CLI's model flag (OpenCode: `provider/model`). |
| `label` | string | Display name, e.g. `Claude Haiku`. |
| `description` | string or null | From the CLI when it reports one. |
| `efforts` | string[] | Accepted reasoning efforts (empty: the model has no effort setting). |
| `default_effort` | string or null | The CLI's default effort, if known. |
| `source` | string | `config` or `discovered` (reported by the CLI). |
| `hidden` | bool | The CLI hides this model from its own picker (still usable). |
| `images` | bool or null | Accepts images, when the CLI says (OpenCode); `null` means the harness capability decides. |
| `local` | bool | Served from this machine or the local network (Ollama, LM Studio, a LAN URL). |
| `provider` | string or null | Upstream provider when the CLI reports it (OpenCode). |

Only enabled harnesses contribute models: the configured `models` (Claude
and Gemini default to the CLIs' own aliases, which resolve to the newest
version), plus, with `discover_models`, what the CLI reports: codex's
`$CODEX_HOME/models_cache.json` (checked by modification time every minute)
and `opencode models --verbose` (at every harness probe).

### Slot

| Field | Type | Notes |
|---|---|---|
| `slot` | string | `chat`, `chat_strong`, `background`, `feed`, `classifier`. |
| `description` | string | What the slot is for. |
| `selection` | object | `{"mode": "automatic"}` or `{"mode": "pinned", "model", "effort"}`. |
| `recommended` | string or null | Highest-ranked available model ("Automatic (recommended: ...)"). |
| `recommended_label` | string or null | Its display name. |
| `effective` | string or null | What turns use now: the pin if usable, else `recommended`. |
| `pin_unavailable` | bool | Pinned, but the pinned model is not usable right now. |
| `status` | string | `ok`, `no_model` (nothing available), or `paused_policy`. |
| `models` | object[] | Available models, best first: `id`, `label`, `harness`, `provider`, `family` (ranking family id or `null`), `known` (matched a family), `local`, `cost` (`free`, `low`, `medium`, `high`, `unknown`), `efforts`, `default_effort`, `images`, `rank` (1-based), `recommended`, `allowed`. |
| `shared` | object | `chat` only: `{"effective", "paused", "allow_claude_for_shared"}` for conversations with more than one person. |

### LoginStatus

| Field | Type | Notes |
|---|---|---|
| `id` | string | Session id. |
| `harness` | string | |
| `flow` | string | `code_paste` or `device_code`. |
| `state` | string | `starting`, `waiting_for_code`, `waiting_for_approval`, `verifying`, `connected`, `failed`, `cancelled`. |
| `url` | string or null | Where to approve. Cleared when the session ends. |
| `user_code` | string or null | Device flow one-time code. Cleared when the session ends. |
| `error` | string or null | Friendly reason (rejected code, timeout after 15 minutes, ...). |
| `started_at` | int | |

### RouteDecision

Stored as `meta.routing` (plus `notes`).

| Field | Type | Notes |
|---|---|---|
| `router` | string | `automatic`, `pinned`, or `fallback` (the slot's pin was not usable for this turn). |
| `model` | string | Catalog id. |
| `effort` | string or null | `null` when the model has no effort setting. |
| `slot` | string | Slot used: `chat`, `chat_strong`, `background`, or `feed`. |
| `escalated` | bool | Moved from `chat` to `chat_strong` for a clearly hard request. |
| `score` | int | Classifier score. |
| `signals` | string[] | Classifier signals that fired, strongest first (see [Effort](#effort)). |
| `reason` | string | Human-readable explanation (signals and notes). |

### Activity

| Field | Type | Notes |
|---|---|---|
| `id` | string | Stable per tool call. |
| `kind` | string | `tool`, `command`, `mcp`, `web_search`, or `file_change`. |
| `name` | string | Tool name as reported by the CLI, e.g. `Read`, `Bash`, `WebSearch` (Claude) or `shell`, `edit`, `web_search` (Codex), or `<server>/<tool>` for MCP tools (`iris/create_goal`). |
| `status` | string | `started`, `completed`, or `failed`. |
| `summary` | string | Short one-line summary (path, command, query); may be empty. |
| `auth_error` | bool | Only present (as `true`) on a failed MCP tool call whose error looks like a rejected or expired sign-in. |

### Settings

| Field | Type | Notes |
|---|---|---|
| `agent_name` | string | Setting, or `agent_name` from config (default `Iris`). |
| `avatar_url` | string | `/api/avatar?v=<n>`; `v` changes when the avatar does (`default` when none is set). |
| `avatar_custom` | bool | A custom avatar is set. |
| `allow_claude_for_shared` | bool | Effective opt-in (config or app setting) for Claude in shared conversations. |
| `feed_prompt` | string \| null | Effective feed prompt (default if unset); `null` for everyone but the owner, who also gets it in `settings.changed` events. |
| `paused` | bool | "Pause Iris" is on (see [`PUT /api/settings`](#put-apisettings-owner)). |
| `voice` | object | Always `{"enabled": false, "status": "not_implemented"}`. |
| `paused` | bool | The owner paused Iris. |

---

## 7. Engine selection

End users never pick models. Iris is model-agnostic: it works with any one
of the four CLIs or any mix, and never hard-codes a model.

### Slots

Every turn uses a **slot**:

| Slot | Used for |
|---|---|
| `chat` | Everyday conversations. |
| `chat_strong` | Chat messages the classifier finds clearly hard (escalation). |
| `background` | Heartbeat and other scheduled jobs. |
| `feed` | Feed generation. |
| `classifier` | Optional helper for ambiguous effort decisions (off by default). |

A slot's **effective** model is its pin, if the owner pinned one and it is
usable here, otherwise the **recommended** model: the highest-ranked model
that is available on this machine right now (its CLI is enabled, installed,
and not signed out, and the CLI lists the model) and allowed by policy. This
is computed for every turn from in-memory state, so installing a CLI,
signing in, or a CLI reporting a new model changes automatic slots at once;
the scheduler checks every minute and broadcasts `models.changed` when a
recommendation moves. Pinned slots never change silently.

### Rankings

The built-in table (`templates/model_rankings.toml`, embedded in the binary;
operators can layer their own with `router.rankings_file`) defines model
**families** by glob patterns over model names (`haiku`,
`claude-*-haiku*`, `gpt-*-luna*`, `gemini-*flash*`, `qwen*`, `llama*`, ...)
with a provider, a cost tier, the harnesses that can run them (in
preference order), and optional filters (`local`, minimum parameter count).
Each slot lists families in order of preference:

| Slot | Preference (abridged) |
|---|---|
| `chat` | Claude Haiku, GPT Luna, Gemini Flash, local models, then other small and open-weight families, then the larger models |
| `chat_strong` | Claude Sonnet, GPT Sol, Gemini Pro, large local models (30B+), Claude Opus, GPT Astra, open-weight families, then the small models |
| `background`, `feed` | Claude Haiku, GPT Luna, Gemini Flash, local models, then the rest |
| `classifier` | Gemini Flash-Lite, GPT mini, Claude Haiku, GPT Luna, Gemini Flash, local |

Within one family, newer versions rank first (`gpt-7-luna` over
`gpt-6.1-luna` over `gpt-6-luna`; `qwen4` over `qwen3`), then the preferred
harness (e.g. `codex` before `opencode` for GPT models). Different families
sharing a position (such as "local") follow the slot's family order. Models
that match no family still appear, ranked after all known ones. Claude and
Gemini are configured with the CLIs' own aliases (`haiku`, `flash`), which
the CLIs resolve to their newest version.

### Effort

The default effort for any model with an effort setting is `low`. A
deterministic, zero-cost rule classifier (no model call) scores each chat
message:

| Signal | Points | When |
|---|---|---|
| `code_block` / `error_output` | 3 | Fenced code, or a stack trace / compiler error. |
| `code`, `research`, `planning`, `math` | 3 each | Whole-word keywords (debug, refactor, sql, ...; research, compare, analyze, ...; plan, design, itinerary, budget, ...; prove, calculate, solve, equations). |
| `asked_for_depth` | 3 | "think hard", "think carefully", "step by step", "be thorough", ... |
| `multi_step` | 2 | Three or more list items, or "for each", "and then", "several", ... |
| `long_input` / `very_long_input` | 2 / 4 | At least `long_chars` (1,200) / `very_long_chars` (6,000) characters. |
| `many_questions` | 1 | Three or more question marks. |
| `correction` | 3 | "that's wrong", "try again", "doesn't work", "still broken", "think harder", "no, ...". |
| `previous_failed` | 2 | The last reply ended in an error. |
| `same_problem` | 1 to 2 | Recent user messages share most content words with this one. |
| `carryover` | 1 to 2 | A follow-up (short, repeated, or a correction) after a `medium` / `high` reply. |
| `casual` | resets | Small talk ("thanks", "ok cool", "good morning") returns straight to `low`. |

A score of 3 or more is `medium`, 6 or more is `high`, and 7 or more is
**clearly hard**: the turn moves from `chat` to `chat_strong`. The effort is
then clamped to what the model offers (the nearest level at or below; a
model whose lowest level is above `low` gets that level; a model without an
effort setting gets none). Scheduled jobs use `low`. With
`router.classifier_model = true`, messages scoring 2 to 5 are also sent to
the `classifier` slot's model as one tiny, tool-less request whose one-word
answer (`low`, `medium`, `high`) sets the effort (signal
`model_classifier`).

### Images and capabilities

For a message with images, the router prefers the first ranked model that
can see images (`images` not `false`). If none can, the turn runs without
the images and the agent tells the user conversationally. A CLI without MCP
support would run without the bridge and connectors (all four have it).

### Individual-use policy

Claude subscriptions are for one person. **Conversations with more than one
member never use Claude** unless the owner opts in
(`router.allow_claude_for_shared` in config, or `allow_claude_for_shared` in
`PUT /api/settings`). The owner's own scheduled jobs are ordinary individual
use and may use Claude. "More than one member" is literal: a group whose
only member is its creator counts as personal.

If Claude is the only usable engine, shared chats pause: turns there fail
with `paused_policy`, and the first time per conversation an `action` card
offers the owner the opt-in. Everything else keeps working. A pinned Claude
slot in a shared chat falls back to the next allowed model (`router:
"fallback"`, reason "individual-use policy").

Shared conversations are also isolated from the owner's data: their turns
get no connectors, a cards-only bridge (see
[section 8](#8-the-iris-mcp-bridge)), a system prompt with only `IDENTITY.md`
and `SOUL.md` of the persona files (no `USER.md`, `TOOLS.md`,
`PROACTIVE_PREFERENCES.md`, `MEMORY.md`, or memory index), and no write
access to the memory folder.
The CLI's own read tools are governed by its permission settings, so treat
group chat as a preview until it gets its own access model.

### Harness availability and fallback

A harness is usable when it is enabled in config, its executable was found,
and its last known sign-in state is not `auth_required`. Signed-out or
missing harnesses are skipped, so the turn moves to the next-ranked model,
and `meta.routing.reason` records when the slot's favorite is down
(`"claude:haiku unavailable, using codex:gpt-6-luna"`). If a CLI reports it
is signed out during a turn (before any text was streamed), Iris marks it
`auth_required`, broadcasts `harness.status`, posts a reconnect card (once
per outage, for a CLI the owner has used), and retries on the next engine.
The harness becomes usable again after a probe or sign-in reports it signed
in.

### Per-harness notes

| Harness | System prompt | MCP | Effort | Images | Resume |
|---|---|---|---|---|---|
| `claude` | `--append-system-prompt-file` (+ `--system-prompt-snapshot off`) | `--mcp-config` file | `--effort` | stream-json content blocks | `--resume` |
| `codex` | on stdin in `<iris_instructions>` before the first message of each thread (`-c developer_instructions` only points to it, so private files stay off argv) | `-c mcp_servers.*` | `-c model_reasoning_effort` | `-i` | `exec resume` (new session when the persona changes) |
| `gemini` | `GEMINI_SYSTEM_MD` file (replaces Gemini's coding prompt; lists tools) | per-conversation `.gemini/settings.json`, servers trusted | none | `@path` references | `--resume <session id>` |
| `opencode` | `instructions` in a private `OPENCODE_CONFIG` file | same file (`mcp`) | `--variant` | `-f` (per-model support from `opencode models --verbose`) | `-s <session id>` |

OpenCode runs without an OS sandbox, so shell commands stay behind `ask`
permission (auto-rejected in headless runs) unless the operator sets
`sandbox = "danger-full-access"`; file edits are allowed in the workspace and
memory folders.

---

## 8. The iris-mcp bridge

`iris mcp` is a small stdio MCP server that the harness CLI launches for each
turn. It is a thin CRUD bridge into the Iris database so the agent can update
the app (goals, ideas, feed, library, cards). It has no shell, web, or file
access other than reading a file to save into the Library.

### How it is launched

Every turn passes this MCP server to the CLI under the server name **`iris`**
(reserved; connectors cannot use it), alongside one connector gateway proxy
(`<iris> mcp-gateway --data-dir <DATA_DIR> --connector <name>`, with the
turn's token in `IRIS_GATEWAY_TOKEN`) per usable connector (none in shared
conversations or other people’s chats; see [3.12](#312-connectors)). Per-call timeouts are raised
for the proxies so a call can wait for the owner's approval (Claude Code
`timeout` plus `CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS=0`, Codex
`tool_timeout_sec`/`startup_timeout_sec`, Gemini CLI and OpenCode
`timeout`).

```
<path to iris binary> mcp --data-dir <DATA_DIR> --user <USER_ID> --conversation <CONVERSATION_ID> [--shared] [--workdir <DIR>]
```

- `--user` is whose data the tools operate on: the conversation's creator for
  chat turns (so in a group conversation, its creator), the owner for jobs.
- `--conversation` is the conversation of the turn (cards are posted there,
  library items are linked to it). Without it, `create_card` fails.
- `--shared` is added for conversations with more than one member. In shared
  mode `tools/list` returns only `create_card`, and calling any other tool
  returns `isError: true` with `Error: <tool> is not available in shared
  conversations`. This keeps other members from reading or changing the
  creator's goals, ideas, feed, library, or connector list through the agent.
- `--workdir` is always the current conversation's directory. Relative
  `save_to_library` paths resolve there; a different directory is refused.
  Neither another conversation nor the memory tree is a library file root.
- `list_connectors` returns an empty list unless this is the owner's private
  context. `read_memory` / `write_memory` are also owner-only and unavailable
  in shared chats; paths must be relative Markdown paths inside Iris memory.
- In split mode the CLI starts `iris mcp-relay SOCKET` with an expiring token
  supplied through per-turn config/environment. Iris runs the real bridge
  as its service user; the agent never opens its database.
- The command refuses to start if there is no Iris database in `DATA_DIR`.

Claude Code receives it through `--mcp-config` (and `mcp__iris` is
pre-approved in `--allowedTools`); Codex receives it through `-c
mcp_servers.iris.*` overrides. Tool calls show up in activity as
`iris/<tool>`.

### Protocol

JSON-RPC 2.0, one JSON message per line on stdin/stdout (MCP stdio
transport). Blank lines are ignored. Batches (JSON arrays) are not
supported.

Supported protocol versions: `2025-11-25`, `2025-06-18`, `2025-03-26`,
`2024-11-05`. On `initialize`, if the client's `protocolVersion` is one of
these it is echoed back; otherwise the server answers `2025-06-18`.

| Method | Result |
|---|---|
| `initialize` | `{"protocolVersion", "capabilities": {"tools": {"listChanged": false}}, "serverInfo": {"name": "iris", "version": "<iris version>"}, "instructions": "Iris app bridge: ..."}` |
| `ping` | `{}` |
| `tools/list` | `{"tools": [...]}` (each with `name`, `description`, `inputSchema`; schemas set `additionalProperties: false`) |
| `tools/call` | Tool result, see below |
| `resources/list` | `{"resources": []}` |
| `prompts/list` | `{"prompts": []}` |
| any message without `id` | Treated as a notification (e.g. `notifications/initialized`); no response |
| anything else | Error `-32601 method not found` |

Unparseable lines get `{"jsonrpc": "2.0", "id": null, "error": {"code":
-32700, "message": "parse error"}}`.

### Tool results

Success:

```json
{
  "content": [{"type": "text", "text": "{\"goal\":{...}}"}],
  "structuredContent": {"goal": {...}},
  "isError": false
}
```

`content[0].text` is the compact JSON serialization of `structuredContent`.

Failure (validation errors, unknown ids, unknown tools): no
`structuredContent`, and

```json
{"content": [{"type": "text", "text": "Error: title is required"}], "isError": true}
```

### Tools

Only `create_card` is offered in shared mode. System status and setup request tools
require an authenticated, active owner private turn; they are unavailable in other
people's chats, standalone bridge invocations, and shared turns. Iris rechecks
membership on calls and expires access with the turn. In split mode the bridge
and status handler run on the service side; no agent process opens private files.

Private system prompts include the same compact status projection. Shared and
isolated turns receive only Iris's version and whether the current engine is usable.
Status reads cached official CLI probe results and current DB/runtime state; it
never runs a CLI probe or operation. `signed_in: null` means not yet known.
Status includes `iris_version` and `iris_build_id` (the same version/build values
as `/api/health`, also supplied to shared and isolated turns), and
`google_setup` (step, total_steps, phase, more_access, summary, authorized_domain, non-secret
project_id, and fixed
error step/message only), engine names/install/sign-in/version, current engine and model,
isolation, connector names/status/rules, saved login host names (the owner can fill those while in control of the browser), wallet card count,
agent name, feed/Pause/quiet hours/voice state, eight recent pending card summaries
(type/title only), and eight recent conversation system notes. It excludes secrets,
account identities, paths, credential labels/usernames/numbers, connector configs,
and card bodies or authorization URLs.

Requests create the app's ordinary owner cards in the current conversation. They
never install, sign in, or connect. Repeated open requests return the same card id;
a persistent owner budget allows six new request cards per hour across turns and
conversations. Existing state returns `already signed in`, `already installed`, or
`already connected`. Invalid targets, denied scope, and exhausted budgets return
`not allowed`. Completion resolves all outstanding matching cards as `done` (✓)
and records `system_note` outcomes in their conversations; declines and approval
decisions also record notes. Notes appear in subsequent context without starting
a new turn. Resolved cards are explicitly described as resolved in replay history.

| Tool | Arguments | Result (`structuredContent`) |
|---|---|---|
| `system_status` | none; owner private turn only | The allowlisted status object described above |
| `request_engine_signin` | `engine: "claude" \| "codex" \| "gemini" \| "opencode"` | `{"outcome", "card_id"?}`; sign-in card or already signed in |
| `request_engine_install` | `engine` (same names) | `{"outcome", "card_id"?}`; install card or already installed |
| `request_connector_setup` | `name`: a catalog id (`"google"`, `"github"`, `"notion"`, `"todoist"`, `"linear"`, `"sentry"`, `"stripe"`, ..., `"custom"`; the tool's schema lists every one) | `{"outcome", "card_id"?}`; connect/reconnect card or already connected (a Google connection that `needs_more_access` gets an "Add more Google services" `connector_reconnect` card instead); paused/disabled connectors return `connector paused by owner` / `connector disabled by owner` with a Resume action card |
| `list_goals` | `include_done?: bool` (default false) | `{"goals": [Goal]}` |
| `create_goal` | `title: string` (required, max 300), `status_text?: string` (max 2,000), `suggested?: bool` (a proposal the user confirms; default false) | `{"goal": Goal}` |
| `update_goal` | `id: string` (required), `title?` (non-blank, max 300), `status_text?` (max 2,000), `done?: bool` | `{"goal": Goal}`; error `no goal with that id` |
| `list_ideas` | `status?: "open" \| "started" \| "done" \| "dismissed"` | `{"ideas": [Idea]}` (max 200) |
| `create_idea` | `title` (required, max 300), `description?` (max 2,000), `prompt?` (max 4,000; sent when the user taps the idea) | `{"idea": Idea}` (status `open`) |
| `list_feed_posts` | `limit?: int` (1 to 50, default 10) | `{"posts": [FeedPost]}` |
| `create_feed_post` | `title` (required, max 300), `body` (required, Markdown, max 50,000), `topic?` (max 100), `source_url?`, `source_name?` (max 100), `image_url?` (http(s), max 2,000), `images?: string[]` (the page's original photos, http(s), max 10), `why?` (max 300) | `{"post": FeedPost}` |
| `read_memory` | `path` (required, relative `.md` path, owner private context only) | `{"content": string}` |
| `write_memory` | `path`, `content` (required, Markdown, owner private context only) | `{"written": true}` |
| `save_to_library` | `title` (required, max 300), exactly one of `content` or `path`, `mime?`, `kind?: "artifact" \| "media"`, `description?` (max 2,000) | `{"item": LibraryItem}` |
| `list_library` | `kind?: "artifact" \| "media"` | `{"items": [LibraryItem]}` (max 500) |
| `create_card` | `type` (required, one of the [card types](#card)), `title` (required), `body?`, `url?`, `image_url?`, `action_label?`, `service?`, `ref_id?` (id of a related goal, idea, or feed post) | `{"card_message_id": "...", "shown": true}` |
| `list_connectors` | none | `{"connectors": [{"name", "service", "description", "enabled", "signed_in", "paused", "rules"}], "available_to_connect": [{"service", "name", "description"}]}` |

`create_card` validates with the [Card](#card) rules and inserts a card
message (`role: "assistant"`) into the current conversation. Clients receive
it as `message.created` right after the turn's `message.completed`.

### The browser tools

In the owner's personal (not shared) conversations and jobs, when a browser
is available and Iris is not paused, each turn also gets a second MCP server
named **`browser`** (tool prefix `mcp__browser__`, activity names
`browser/<tool>`). `browser` and `iris_browser` are reserved for new
connectors; on an install that already has a connector named `browser`, that
connector keeps working and Iris's server is named `iris_browser` instead.
It is launched as:

```
<path to iris binary> mcp --browser --data-dir <DATA_DIR>
```

with `IRIS_BROWSER_TOKEN` in its environment. That process only forwards
`tools/list` and `tools/call` over a private Unix socket
(`<data dir>/run/browser.sock`, or a private directory under
`$XDG_RUNTIME_DIR` when that path is too long) to the server; the token
identifies the turn and stops working when the turn ends. The server
forwards each call to [Iris's browser](#319-iriss-computer-browser), the
Playwright MCP server it runs, and returns that server's result unchanged
(text, and `image` blocks for screenshots). The tools, their arguments, and
their results are Playwright MCP's own; see its documentation. Action
results do not include the page, so the agent calls `browser_snapshot` to
read it. Iris decides nothing; it only refuses:

- Tools outside the reviewed list. The agent gets `browser_navigate`,
  `browser_navigate_back`, `browser_tabs`, `browser_snapshot`,
  `browser_find`, `browser_take_screenshot`, `browser_click`,
  `browser_hover`, `browser_drag`, `browser_type`, `browser_fill_form`,
  `browser_select_option`, `browser_press_key`, `browser_handle_dialog`,
  `browser_wait_for`, and the position-based `browser_mouse_click_xy`,
  `browser_mouse_drag_xy`, `browser_mouse_move_xy`, `browser_mouse_down`,
  `browser_mouse_up`, `browser_mouse_wheel` (CSS pixels of the 500 x 1000
  page; for CAPTCHAs, frames from other sites, maps, canvases). Not
  offered, and refused: `browser_run_code_unsafe` (code inside the server,
  on Iris's side), `browser_evaluate` (page scripts can read cookies and
  stored tokens), `browser_file_upload` and `browser_drop` (files on the
  server), `browser_network_request(s)` (headers carry cookies),
  `browser_console_messages`, `browser_resize`, `browser_close`, and any
  tool a newer version adds.
- Writing results to files: the `filename` option is removed from the
  definitions and refused, and so are output options in `_meta`.

Iris adds two tools of its own, after Playwright's:

- `fill_saved_login` (`username_ref`, `password_ref`, optional `login`,
  `submit`, `submit_ref`): types the owner's saved login for the current site into the
  fields with those element refs (from `browser_snapshot`). Each field's own
  frame and the page must be the site the login was saved for, over HTTPS
  (plain HTTP only on this machine), and the password goes only into a
  password field. `submit` presses Enter in the last field; `submit_ref`
  clicks that button instead (it must be on the same site).
- `fill_saved_card` (`number_ref`, optional `exp_ref`, `exp_month_ref`,
  `exp_year_ref`, `cvc_ref`, `name_ref`, `card`, `submit`, `submit_ref`): only after the
  owner approves an approval card in the chat (service `wallet`; the same
  pending map, expiry, and cancellation as connector approvals). The fields
  are found first, and the card names the page and any other site whose
  frame holds a field (a payment provider); the fill goes only to that page
  and those origins (scheme, host, port), every field over HTTPS. One
  approval fills once; a card edited since needs a new one. `submit_ref`
  (a pay button) must be on the approved page or one of those frames; the
  form is sent (Enter or the button) only when every requested field was
  filled.
  Expiry lists are matched against their own options (value, label, number,
  or month name); a year field's hints (`YY`) choose 31 over 2031.

Iris locates each field and types its value through Playwright's own
`browser_run_code_unsafe` on Iris's side (never offered to the agent); the
values never appear in a tool result, message, log, or command line. The
answers are `filled`, `filled and submitted`, `denied`, `not filled (...)`,
or `not found (...)`. Filling a password, card number, expiry, or CVC closes
the page to the agent as if the owner had typed it (see below); the fill
tools themselves stay usable there. The owner's saved passwords and card
numbers, and every value Iris filled this browser session (a CVC only
where it stands alone), are replaced with `[hidden]` in every result the
agent gets (addresses, titles, dialogs, page text; also their common
encodings and card numbers written in groups). While the page's text or
fields (all frames) show one, screenshots, `browser_find`,
`browser_wait_for`, and targets written as selectors instead of snapshot
refs are refused (their outcome would tell what is hidden); a screenshot
is checked again once taken and dropped if the page became private or
showed one meanwhile. Expiry dates
and names lock the page but are not hidden elsewhere. Playwright writes no
page snapshots to files (`snapshot.mode: none`); `browser_snapshot`
answers inline, and Playwright's output folder (screenshot files, its
console logs) is emptied after every call, once a minute, and shortly after
an abandoned call.
- Addresses that are not web pages in `browser_navigate` and
  `browser_tabs` (`url`): only `http`, `https`, a bare domain without a
  port, or `about:blank`; anything with whitespace or control characters
  (browsers drop them while parsing) is refused.
- Anything but `browser_navigate`, `browser_navigate_back`, `browser_tabs`,
  `browser_handle_dialog`, and the fill tools while the current page holds
  private details the owner typed or filled (see [Input while in
  control](#4-websocket-get-apiws)).

Each call waits while the owner has taken over (up to about 50 seconds,
then tells the agent the user is still using it), and checks again once it
holds the page, so a queued call never acts under the owner's control.
Takeover also discards the result of a call already under way, even if
control is immediately handed back. When the turn ends (finished,
cancelled, or failed), its calls stop at once, even in the middle of a
wait. Reads (`browser_snapshot`, `browser_find`, `browser_take_screenshot`,
`browser_wait_for`) are dropped at once. An action already sent to the
browser keeps the page until it finishes (Playwright cannot stop one; at
most the call's 90-second limit), so it never lands under the owner's
hands; opening a page (`browser_navigate`, `browser_navigate_back`) keeps
it for at most 8 seconds. A file picker the agent's action opens is
cancelled (the result says so); the agent answers its own dialogs with
`browser_handle_dialog`. Every harness gets a per-server timeout of the effective approval
window plus 300 seconds, using the standard per-server MCP timeout fields.

The agent never handles passwords, card numbers, or codes: its instructions
tell it to ask the owner to take over for sign-ins and payments (always for
sites that fight automated browsers, such as X).

### `save_to_library` details

- **`content`** (text, up to 1,000,000 characters): `mime` defaults to
  `text/markdown` and must be `text/*` or `application/json`. The file
  extension is `md`, `html`, `csv`, `json`, or `txt` based on `mime`.
- **`path`**: a relative or absolute path inside the **current conversation's
  working directory**, naming a regular file up to 20 MiB. Every path
  component is opened relative to a directory descriptor without following
  symlinks; traversal, symlinked directories, other conversations and memory
  files are refused. In split mode the fixed launcher reads the file as the
  agent user. `mime` defaults from the extension (`md`, `txt`, `csv`, `json`,
  `html`, `pdf`, `png`, `jpg`/`jpeg`, `gif`, `webp`, `svg`, `mp3`, `wav`, `mp4`;
  otherwise `application/octet-stream`).
- `kind` defaults to `media` for `image/*`, `audio/*`, `video/*`, else
  `artifact`.
- The file is copied into the Library folder; the item is linked to the
  current conversation.

### Entity change notifications

The bridge runs in a separate process and does not emit events itself. When
the turn ends, Iris looks at the turn's `mcp` activities named
`iris/<tool>` with status `completed`, skips the read-only `list_*` tools,
and emits one `entities.changed` to the turn's `--user` with `goals`,
`ideas`, `feed`, and/or `library` for tool names containing `goal`, `idea`,
`feed`, or `library`. This also happens for job turns, which emit no other
conversation events.

---

## 9. Push notifications

Iris sends Web Push (RFC 8030) with VAPID (RFC 8292) and `aes128gcm`
encryption (RFC 8291). Push is enabled by default (`[push] enabled`) but
needs a contact for push services: `vapid_subject` (a `mailto:` or
`https:` URL), or else an `https:` `public_url`. Without one,
`GET /api/push/vapid-public-key` reports `enabled: false` with a `reason`
and subscribing fails with 400. The VAPID key pair is generated on first
use.

### Subscribing (browser)

1. `GET /api/push/vapid-public-key` and pass `public_key` as
   `applicationServerKey` to `registration.pushManager.subscribe({userVisibleOnly: true, applicationServerKey})`.
2. `POST /api/push/subscriptions` with `subscription.toJSON()`.
3. Optionally `POST /api/push/test`.

### When pushes are sent

| Trigger | Recipients | `kind` |
|---|---|---|
| Heartbeat job posts a message in the Main chat | The owner, always (online or not) | `heartbeat` |
| A chat turn completes successfully (`status: "complete"`) and `push.notify_replies_when_offline` is true (default) | Conversation members with **no connected WebSocket** at that moment | `reply` |
| A chat turn fails (`status: "error"`), same setting | Same | `stuck` (body "I got stuck on that. Tap to try again.") |
| A CLI the owner used went signed out (reconnect card posted) | The owner | `engine_signin` |
| A connector refused its sign-in (reconnect card posted) | The owner | `connector_reconnect` |
| An urgent personal feed item: a price drop of 10% or more on a saved product, or an event soon near the user that matches a core interest (at most 2 a day) | The owner | `feed_urgent` |
| Quiet hours ended with notifications held | The user | `digest` (or the single held notification with `digest: true`) |
| A connector call waits for approval (approval card posted) | The owner | `approval` |
| A tripwire paused a connector | The owner | `safety` |
| `POST /api/push/test` | The caller | `test` |

Ordinary feed posts never push. Cancelled and scheduled turns do not
trigger reply pushes.

### Quiet hours

Notifications for a user inside their
[quiet hours](#put-apimequiet-hours) are held in the database (at most 50
per user; test pushes are sent at once). The minute scheduler delivers them
in the background as soon as the quiet hours end (or are turned off): one
held notification is sent as-is; several become one digest, `"While it was
quiet: N updates. <latest few>"`, opening their chat when they all came
from one. Held replies from chats the user has read since are left out.
Notifications from chats the user is no longer a member of are dropped.
The held rows are removed once the digest reaches at least one device (or
the user has none); if every device fails, the next tick tries again, and
notifications still undelivered after 48 hours are dropped. While push is
turned off in the config nothing is delivered or dropped.

### Payload

The decrypted payload is a JSON object:

```json
{
  "title": "Iris",
  "body": "Your package is out for delivery and should arrive by 5pm…",
  "conversation_id": "...",
  "message_id": "...",
  "kind": "reply"
}
```

| Field | Notes |
|---|---|
| `title` | The agent name. |
| `body` | Message text cut to 180 characters (with `…` when cut). Test: `Notifications are working.` |
| `conversation_id`, `message_id` | Omitted in test pushes. |
| `kind` | `reply`, `stuck`, `heartbeat`, `engine_signin`, `connector_reconnect`, `feed_urgent`, `digest`, `approval`, `safety`, or `test`. |
| `url` | Optional app path to open (`/#/foryou` for feed pushes). |
| `digest`, `count` | Present on notifications delivered after quiet hours. |

Requests use `TTL: 86400` and `Urgency: normal`.

### Subscription cleanup

- A `404` or `410` from the push service deletes the subscription
  immediately.
- Other failures increment `failure_count`; a subscription is deleted after
  **10** consecutive failures. A success resets the count and sets
  `last_success_at`.
- Revoking a device deletes its subscriptions.

---

## 10. Placeholders and TODO

- **Voice**: `POST /api/voice/speak` always returns `501 not_implemented`,
  and `settings.voice` is `{"enabled": false, "status": "not_implemented"}`.
  The planned shape is `POST /api/voice/speak {"message_id"}` returning
  `audio/mpeg` from a local TTS engine.
- **Approvals for the CLIs' own tools**: connector calls go through the
  approval gate. The CLIs' built-in tools (shell, file edits, web fetch)
  are limited by each CLI's sandbox and permission settings
  ([Security model](SELF-HOSTING.md#security-model)) rather than by
  approval cards.
- **Browser limits**: logins and cards in cross-origin frames (for example
  embedded payment widgets from another domain) cannot be filled; the agent
  is told `not found` and can ask the owner to take over.

---

## 11. The app

The Iris phone app is plain ES modules (Preact and htm vendored under
`web/vendor`, no build step) embedded in the binary and served at `/` with
a strict Content-Security-Policy (`script-src 'self'`). It is mobile first,
follows the phone's light or dark theme, and installs to the home screen
(`/manifest.webmanifest`). The service worker (`/sw.js`) caches the app
shell for offline start (never `/api`), shows push notifications, and opens
the right chat when one is tapped. The server renders its cache name as
`iris-shell-<version>-<bundle hash>`, hashing the paths and bytes of all embedded
web assets. Any web change creates a new cache; unchanged bundles keep the same
name regardless of build ID.

The app only uses the routes and events in this document. It stores its
device token in `localStorage` and fetches binary routes with the
`Authorization` header (object URLs). End users never see model names,
efforts, or tokens outside Settings, AI engine.

Iris's computer (`#/iris/computer`, owner only) shows the live browser,
Watch (full screen) and Take over (taps, drags as scrolling, an address bar,
and a keyboard button whose hidden field forwards typing), plus Pause Iris.
While a turn uses the browser, a small live thumbnail (the browser peek)
sits under that message in chat; it disappears when the turn ends.

For development, `IRIS_WEB_DIR=web iris serve` serves the app files from
disk so changes need no rebuild. `iris demo-seed --assets
scripts/demo/images` fills a fresh data directory with fake demo content
(drawn illustrations, fake people and places) for screenshots.

