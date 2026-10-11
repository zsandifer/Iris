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
4. [WebSocket: `GET /api/ws`](#4-websocket-get-apiws)
5. [Turn lifecycle](#5-turn-lifecycle)
6. [Data model](#6-data-model)
7. [Engine selection](#7-engine-selection)
8. [The iris-mcp bridge](#8-the-iris-mcp-bridge)
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
| `voice_failed` | 502 | The local voice program could not make a voice note or transcribe a recording. The message is first-person and friendly. |
| `voice_unavailable` | 503 | `POST /api/voice/speak` or `/api/voice/transcribe` while that voice component is not installed on the server. |
| `not_found` | 404 | Unknown resource (message is `<thing> not found`), or an unknown route (`route not found`). |
| `conflict` | 409 | Creating a connector whose name already exists. |
| `too_large` | 413 | Uploaded image larger than `max_upload_bytes`. |
| `unsupported_media_type` | 415 | Uploaded bytes are not PNG, JPEG, GIF, or WebP. |
| `locked` | 429 | Pairing is locked after repeated failures (see [section 2](#2-authentication-and-pairing)). |
| `internal` | 500 | Unexpected server error. Message is always `internal server error`; details only go to the server log. |

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
| everything else under `/api` | `Authorization: Bearer iris_<64 hex chars>` |

The scheme name `Bearer` is case-insensitive. The WebSocket can also
authenticate with a first frame instead of the header (see
[section 4](#4-websocket-get-apiws)).

**Owner-only** routes return `403 forbidden` for other users. They are marked
**Owner** in the route tables. There is exactly one owner (the first user,
created automatically when the database is initialized). The owner adds
other people (for example a partner); each person gets **their own Iris**:
their own persona and memory files, feed, ideas, goals, library, and
settings (name, picture, voice), plus shared "Together" chats. Server-wide
things (engines, models, connectors, saved logins, the shared-chat policy)
stay with the owner. Some group-conversation actions are limited to the
group's creator instead (see [section 3.3](#33-conversations-and-messages)).

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
  open (`public_url` from config, or `http://<bind>`).
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

Failed attempts are counted globally (not per client) in memory:

- After **5 failed attempts within a 15-minute window** (the window starts at
  the first failure), **every outstanding unused pairing code is burned**, and
  further `POST /api/pair` requests get `429 locked` until the window ends.
- A successful pairing resets the counter.
- Burned codes stay invalid after the lock clears; create a new one.
- Restarting the daemon clears the counter (codes already burned stay burned).

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
| GET | `/users` | Any | Everyone on this server |
| POST | `/users` | **Owner** | Add a person (their own Iris) |
| DELETE | `/users/{id}` | **Owner** | Remove a person and their Iris |
| POST | `/users/{id}/pairing-code` | **Owner** | Pairing code for a user |
| GET | `/conversations` | Any | List your conversations |
| POST | `/conversations` | Any | Create a side or group ("Together") conversation |
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
| GET | `/feed` | Any | List feed posts (`?liked=true`: saved posts) |
| POST | `/feed/refresh` | Any | Run your feed job now (`202`) |
| GET | `/feed/prompt` | Any | Read your feed prompt |
| PUT | `/feed/prompt` | Any | Set your feed prompt |
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
| GET | `/files` | Any | Your Iris's system files and folders |
| GET | `/files/system/{name}` | Any | Read one of your system files |
| PUT | `/files/system/{name}` | Any | Write one of your system files |
| GET | `/files/browse/{area}` | Any | List one of your folders |
| GET | `/files/content/{area}` | Any | Read a text file |
| PUT | `/files/content/{area}` | Any | Write a text file |
| DELETE | `/files/content/{area}` | Any | Delete a file or empty folder |
| GET | `/settings` | Any | Read your settings |
| PUT | `/settings` | Any (policy: **Owner**) | Update your settings |
| PUT | `/settings/avatar` | Any | Upload your Iris's picture (raw body) |
| DELETE | `/settings/avatar` | Any | Reset your Iris's picture |
| GET | `/avatar` | Any | An Iris's picture (yours, or a Together chat host's) |
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
| GET | `/credentials` | **Owner** | List credential metadata |
| POST | `/credentials` | **Owner** | Create a credential |
| PATCH | `/credentials/{id}` | **Owner** | Update a credential |
| DELETE | `/credentials/{id}` | **Owner** | Delete a credential |
| GET | `/push/vapid-public-key` | Any | VAPID public key |
| GET | `/push/subscriptions` | Any | Your push subscriptions |
| POST | `/push/subscriptions` | Any | Subscribe this device |
| DELETE | `/push/subscriptions/{id}` | Any | Unsubscribe |
| POST | `/push/test` | Any | Send yourself a test push |
| GET | `/jobs` | **Owner** | Scheduled job status |
| POST | `/jobs/{name}/run` | **Owner** | Run a job now (`202`) |
| POST | `/voice/speak` | Member | A reply as a voice note (WAV) |
| POST | `/voice/transcribe` | Any | Voice input: WAV in, text out |
| GET | `/ws` | token | WebSocket event stream |

"Member" routes return `404 not_found` (`conversation not found`) when the
caller is not a member of the conversation, and for hidden job conversations.

Feed, idea, goal, library, file, and settings routes only ever see the
caller's own Iris; another user's id behaves like an unknown id (`404`).

### 3.2 Health, pairing, me, devices, users

#### `GET /api/health`

No auth.

```json
{"ok": true, "name": "iris", "version": "0.1.0"}
```

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
During quiet hours no push notification is sent to the caller (the messages
still arrive in the app); test pushes are not affected. Per user. Response:
`{"quiet_hours": {...} | null}`.

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

#### `GET /api/users`

Everyone on this server, for starting Together chats.

```json
{"users": [{User}]}
```

Ordered by creation time. For the owner each user also has `devices`, the
number of devices they have paired (not revoked).

#### `POST /api/users` (Owner)

Adds a person with their own Iris: their Main chat is created and their
persona and memory files are seeded from the generic templates (in
`<data dir>/people/<user id>/`). They pair their phone with a code from
`POST /api/users/{id}/pairing-code`.

| Field | Type | Notes |
|---|---|---|
| `display_name` | string | Required, non-blank, max 80. Trimmed. |

Response: `{"user": {User}}` with `is_owner: false`.

#### `DELETE /api/users/{id}` (Owner)

Removes a person and everything that is only theirs: their devices and
push subscriptions, their persona/memory/workspace folder, their chats and
the Together chats they created (running turns are cancelled), feed, ideas,
goals, library items, saved logins, and settings. Their messages in other
people's Together chats stay, without an author. The owner cannot be
removed (`400`); unknown id: `404`. Response `{"ok": true}`.

#### `POST /api/users/{id}/pairing-code`

Owner only. Creates a one-time pairing code for user `{id}`.

```json
{"code": "7KQM-3XPA", "expires_at": 1760000600000}
```

`404` if the user does not exist.

There is no API to revoke another user's single devices; remove the person
instead.

### 3.3 Conversations and messages

Conversation kinds: `main` (one per user, cannot be deleted or archived),
`side` (topic chats), `group` (several people), and `job` (hidden; never
returned by the API).

Each member has a role: `owner` for the person who created the conversation
(its **creator**; not necessarily the Iris owner) and `member` for everyone
added later. Anyone can start a group ("Together") conversation with other
people on the server; the **creator's Iris** (name, picture, `IDENTITY.md`,
`SOUL.md`) answers in it.

A conversation is **shared** when it has more than one member
(`member_count > 1`). A group whose only member is its creator is treated as
personal. Shared conversations change how turns run: the individual-use
policy keeps Claude out unless the owner opts in
([section 7](#7-engine-selection)), no connectors are passed to the CLI,
the Iris bridge only offers cards ([section 8](#8-the-iris-mcp-bridge)),
nobody's private files or memory are in the prompt, and the CLI works in a
folder of its own (`<data dir>/together/<conversation id>`). In personal
conversations the bridge acts on the creator's data.

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
| `kind` | string | `"side"` | `"side"` or `"group"` (a Together chat; anyone may start one, and their Iris answers in it). |
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
  ],
  "agent": {"user_id": "...", "name": "Iris", "avatar_url": "/api/avatar?user=...&v=default",
            "avatar_custom": false}
}
```

Members are ordered by `joined_at`. `agent` is the Iris that answers here
(the creator's), so a Together chat shows the host's Iris name and picture
on everyone's phone; `null` if the creator was removed.

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

In a shared conversation, the other members whose app is closed (no
connected WebSocket) also get a push notification with the message
(`kind: "together"`, title `<sender> · <chat title>`).

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

```json
{"attachments": [{"id": "...", "mime": "image/jpeg", "size": 48213, "sha256": "<64 hex>",
                  "created_at": 1760000000000, "conversation_id": "...", "message_id": "..."}]}
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

Posts are written by the agent (feed job or the `create_feed_post` bridge
tool). Users read, like, delete, and discuss them.

#### `GET /api/feed`

| Query | Type | Default | Notes |
|---|---|---|---|
| `before` | int (ms) | none | Only posts created strictly before this. |
| `limit` | int | 30 | Clamped to 1..100. |
| `liked` | bool | `false` | Only saved (liked) posts. |

```json
{"posts": [{FeedPost}], "has_more": false}
```

Newest first. To page, pass the `created_at` of the last (oldest) post as
`before`. `has_more` is `true` exactly when older posts exist.

#### `POST /api/feed/refresh`

Starts the caller's own feed job now. Response **`202`**: `{"started":
true}`; `started` is `false` if that person's feed job was already running.
Runs even if the feed job is disabled in config.

#### `GET /api/feed/prompt`

```json
{"feed_prompt": "Write a few short, genuinely interesting posts ..."}
```

Returns the built-in default when none is set.

#### `PUT /api/feed/prompt`

Sets the caller's own feed prompt.

| Field | Type | Notes |
|---|---|---|
| `feed_prompt` | string | Required field, max 4,000. Blank resets to the default. |

Response: `{"feed_prompt": "<effective prompt>"}`. Emits `settings.changed`
to the caller.

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
Liked titles steer later feed generation.

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
appended when cut); the message `content` (`Feed post "<title>": <body>`)
carries the full post and becomes context for the first turn. No turn is
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
| `status` | string | Optional exact filter (not validated; an unknown value returns `[]`). |

```json
{"ideas": [{Idea}]}
```

Newest first, at most 200.

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
false}`. Only the card's `approver_user_id` may decide (`403`), only once
(`409 conflict`, "this was already decided"). `404` if the caller is not a
member of the message's conversation; `400` if the message is not an
approval card. Sets the card's `state` to `approved` or `denied` and
`decided_at`, broadcasts `message.updated`, and returns `{"message":
{Message}}`.

### 3.8 Library

Items are saved by the agent through `save_to_library`. Kinds: `artifact`
(documents, notes, code) and `media` (images, audio, video).

#### `GET /api/library`

| Query | Type | Notes |
|---|---|---|
| `kind` | string | Optional exact filter (`artifact` or `media`; not validated). |

```json
{"items": [{LibraryItem}]}
```

Newest first, at most 500.

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

Every person edits their **own** Iris here: the owner's files are the
top-level `persona/`, `memory/`, and `workspace/` folders of the data
directory; everyone else's are under `people/<user id>/`. Nobody can open
another person's files through these routes.

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

Settings are per person: each person's Iris has its own name, picture,
feed prompt, and voice choice. `allow_claude_for_shared` is the one
server-wide setting, and only the owner may change it.

#### `GET /api/settings`

Returns the caller's [Settings](#settings) object directly (not wrapped):

```json
{
  "agent_name": "Iris",
  "avatar_url": "/api/avatar?user=<user id>&v=default",
  "avatar_custom": false,
  "allow_claude_for_shared": false,
  "feed_prompt": "...",
  "voice": {"enabled": false, "tts": false, "stt": false}
}
```

#### `PUT /api/settings`

All fields optional; only supplied fields change.

| Field | Type | Notes |
|---|---|---|
| `agent_name` | string | The caller's Iris. Non-blank, max 40, must not contain newline, carriage return, `<`, `>`, or a backtick. Trimmed. |
| `voice_replies` | bool | The caller hears replies as voice notes plus text (`voice.enabled`). |
| `feed_prompt` | string | The caller's feed. Max 4,000. Blank resets to the default. |
| `allow_claude_for_shared` | bool | **Owner only** (`403` otherwise). Opt-in to let Claude answer for other people: in conversations with more than one person and in other people's own Iris ([individual-use policy](#individual-use-policy)). `false` clears the app setting (the config value still applies). |

Unknown fields are ignored. Models are no longer set here: pin slots with
[`PUT /api/models/slots/{slot}`](#put-apimodelsslotsslot-owner). (An
`engine` pin stored by earlier versions is read as the `chat` slot's pin.)

Response: the caller's updated Settings object. Emits `settings.changed` to
the caller, or to every user (each with their own settings) when
`allow_claude_for_shared` was sent.

#### `PUT /api/settings/avatar`

Sets the picture of the caller's own Iris. Raw image body, validated
exactly like `POST /api/attachments` (PNG, JPEG, GIF, WebP; `413`/`415`
errors); the app crops it to a 512 px square first. The previous custom
picture is deleted. Response: the updated Settings object (`avatar_custom:
true`, `avatar_url` with a new `v`). Emits `settings.changed` to the caller.

#### `DELETE /api/settings/avatar`

Reverts the caller's Iris to the built-in picture. Response: the Settings
object. Emits `settings.changed` to the caller.

#### `GET /api/avatar`

| Query | Type | Notes |
|---|---|---|
| `user` | string | Whose Iris (default: the caller's). Another person's only if the caller shares a conversation with them (a Together chat's host); otherwise `404`. |
| `v` | string | Cache buster only. |

The uploaded image, or the built-in SVG (`image/svg+xml`). Requires auth.

### 3.11 Harnesses, sign-in, models, and slots

#### `GET /api/harnesses`

```json
{"harnesses": [{HarnessStatus}]}
```

Each entry is a [HarnessStatus](#harnessstatus) plus `label` (e.g.
`"Codex (ChatGPT)"`) and `capabilities` (`{"images", "effort", "mcp",
"resume"}`, all bool). For the owner it also has `login_flow`
(`"code_paste"`, `"device_code"`, or `"manual"`), `login_hint` (the one
server step when the flow is manual), and `login` (the current
[LoginStatus](#loginstatus) or `null`). `path` is only filled in for the
owner.

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
card becomes `connected`.

The sign-in runs the CLI's **own** login command on the server. Iris relays
what the user needs, passes the code the user pastes to the process's
stdin, and judges success only by the CLI's own status command. Codes are
never stored or logged, and Iris never reads the credential files the CLI
writes.

| Harness | `login_flow` | Command | What the user does |
|---|---|---|---|
| `claude` | `code_paste` | `claude auth login --claudeai` | Opens `url`, approves, copies the code shown, sends it with `/login/code`. |
| `codex` | `device_code` | `codex login --device-auth` | Opens `url`, enters `user_code`. Finishes by itself. |
| `gemini` | `manual` | (none; Gemini CLI signs in only in an interactive terminal) | Runs `gemini` on the server once (`login_hint`). |
| `opencode` | `manual` | (none) | Runs `opencode auth login` on the server, or configures a local provider. |

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

Connectors are the owner's accounts: MCP servers passed to the harness CLI
while enabled on the owner's own turns (personal chats and scheduled jobs).
Turns in shared conversations (more than one member) and other people's own
Iris get no connectors. All routes are **owner-only**.

Secret values (`env` values and `headers` values) are encrypted at rest and
**never returned**; responses list only their key names. See
[Connector](#connector).

#### `GET /api/connectors`

`{"connectors": [{Connector}]}`, ordered by name.

#### `POST /api/connectors`

| Field | Type | Default | Notes |
|---|---|---|---|
| `name` | string | | Required. 1 to 48 characters of `A-Z a-z 0-9 _ -`, not `iris` (reserved). Unique (`409 conflict`). Used as the MCP server name and tool prefix. |
| `description` | string | `""` | Max 500. |
| `transport` | string | `"stdio"` | `"stdio"` or `"http"`. |
| `command` | string | | stdio: required, max 500. |
| `args` | string[] | `[]` | stdio arguments. |
| `env` | object | `{}` | stdio environment, `{"KEY": "secret value"}`. |
| `url` | string | | http: required. Must start with `http://` or `https://`, max 2,000. |
| `headers` | object | `{}` | http headers, `{"Authorization": "Bearer ..."}`. |
| `enabled` | bool | `true` | |

Keys in `env` and `headers` must be 1 to 100 characters of `A-Z a-z 0-9 _ -`.

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

Same fields except `name` and `transport`, which cannot be changed (they are
ignored if sent). Supplying `env` or `headers` **replaces** the whole map.
Response `{"connector": {Connector}}`.

#### `POST /api/connectors/{id}/connect`, `POST /api/connectors/{id}/disconnect`

Set `enabled` to `true` / `false`. Response `{"connector": {Connector}}`.

#### `DELETE /api/connectors/{id}`

`{"ok": true}`.

### 3.13 Credentials

Saved logins and payment cards. All routes are **owner-only** and scoped to
the caller. Secrets (password, notes, card number, CVC) are encrypted at rest
and **never returned**; responses only contain non-secret metadata. Iris does
not yet give credentials to the agent (see
[section 10](#10-placeholders-and-todo)).

#### `GET /api/credentials`

```json
{"credentials": [{Credential}], "injection": "not_implemented"}
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
| `site` | string | metadata | Max 300. |
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
{"enabled": true, "public_key": "<base64url uncompressed P-256 point>"}
```

`public_key` is `null` when push is disabled in config. Pass it as
`applicationServerKey` to `pushManager.subscribe()`.

#### `GET /api/push/subscriptions`

`{"subscriptions": [{PushSubscription}]}` for the caller.

#### `POST /api/push/subscriptions`

Body: the browser's `PushSubscription.toJSON()`. Extra fields such as
`expirationTime` are ignored.

| Field | Type | Notes |
|---|---|---|
| `endpoint` | string | Required, must start with `https://`, max 2,000. |
| `keys.p256dh` | string | Required, base64url P-256 public key. |
| `keys.auth` | string | Required, base64url 16-byte auth secret. |

```json
{"endpoint": "https://push.example.com/send/abc", "expirationTime": null,
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

Both routes are **owner-only** and show or run the owner's own jobs (every
person's Iris runs its own; people start their feed with
`POST /api/feed/refresh`). Jobs: `heartbeat` and `feed` (see
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

Voice runs on the server with open-source models, so audio never leaves it:
a Piper voice for replies and Whisper (`base.en`) for voice input, both
through [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) (Apache-2.0).
The owner installs them from the app as the `tts` and `stt` engines
([section 3.17](#317-engines-install-and-update)). `settings.voice.tts` and
`settings.voice.stt` say whether they are installed. One synthesis or
transcription runs at a time, as a short-lived process with a 3 minute
limit.

When the server has no voice, the app reads replies aloud with the phone's
own voice (and labels them so); when it cannot transcribe, the app uses the
browser's speech recognition if there is one.

#### `POST /api/voice/speak`

A finished assistant reply as a voice note.

| Field | Type | Notes |
|---|---|---|
| `message_id` | string | Required. A finished `text` reply (`role: "assistant"`) in a conversation the caller belongs to (`404` otherwise; `400` for other messages). |

Response `200`: `audio/wav` (16-bit mono PCM), with `x-voice-duration-ms`
and `Cache-Control: private, max-age=86400`. The spoken text is the reply
without Markdown, code (it says the code is in the text), or URLs, cut at
a sentence near 1,500 characters ("The rest is in the text."). Voice notes
are cached under `<data dir>/voice/` (keyed by message and text) for 30
days; when a reply finishes and anyone in the conversation has voice
replies on, it is made right away so it plays without a wait.

Errors: `503 voice_unavailable` (no server voice), `502 voice_failed`.

#### `POST /api/voice/transcribe`

Voice input. The raw request body is a WAV file (16-bit PCM, mono or
stereo, any sample rate; the app records on the phone and converts to 16
kHz mono WAV before uploading), at most 5 minutes and `max_upload_bytes`.

Response `200`:

```json
{"text": "What is the capital of France?", "duration_ms": 2400}
```

`text` may be empty when nothing was heard. The recording is written to a
private temporary file, transcribed, and deleted; it is never stored or
logged.

Errors: `415 unsupported_media_type` (not 16-bit PCM WAV), `413
too_large`, `503 voice_unavailable` (no speech-to-text on the server),
`502 voice_failed`.

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
| `tts` | `github.com/k2-fsa/sherpa-onnx` releases: the sherpa-onnx runtime for Linux x86_64/aarch64 and the Piper voice `en_US-ljspeech-medium` (public-domain dataset) | SHA-256 and size pinned in Iris for every download |
| `stt` | the same runtime and Whisper `base.en` (only the int8 files are kept) | same |

The npm engines need Node.js on the server; `tts` and `stt` need `tar` and
`bzip2` to unpack. The two voice entries are not AI engines (they never
answer turns): they share one runtime, which is removed with the last of
them, and updating them installs the versions pinned in this Iris release. Iris only ever updates or
removes engines it installed; a CLI the operator installed (for example in
`~/.local/bin`) is reported as `external` and never touched. With
`engines.helper` set, installs run as a separate admin user through that
helper (see `deploy/setup-engines.sh`) and the server itself never downloads
anything.

#### `GET /api/engines`

```json
{
  "engines": [{
    "name": "claude", "kind": "engine", "label": "Claude Code",
    "source": "downloads.claude.ai (Anthropic release manifest, SHA-256 verified)",
    "installed": true, "managed": true, "external": false,
    "version": "2.1.296 (Claude Code)", "managed_version": "2.1.296",
    "auth": "ok", "requires": null,
    "operation": {"op": "update", "state": "done", "error": null, "version": "2.1.296",
                  "started_at": 1760000000000, "finished_at": 1760000017000}
  }],
  "dir": "/home/user/.local/share/iris/engines",
  "privileged_helper": false,
  "auto_update": true
}
```

`kind`: `"engine"` for the AI engines, `"voice"` for `tts` and `stt`.
`installed`: the CLI was found (anywhere). `managed`: Iris installed it.
`requires`: `"nodejs"` when the engine needs npm and it is missing (for
voice: `"tar"` or `"bzip2"`). `operation`: the last install/update/uninstall
(`state` `running`, `done`, or `failed` with `error`), or `null`. Voice
entries also carry `download_mb` (approximate one-time download) and
`unsupported` (a reason, when there is no official build for this server).

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
engines Iris installed once a day, when no turn is running. Claude Code
updates swap a new verified binary in atomically, so running turns are not
interrupted.

#### First run

When the server starts and no engine can answer, Iris posts one
`engine_setup` [card](#card) in the owner's Main chat listing the engines
with install actions. Installing one is followed by its sign-in card; the
setup card becomes `done` once any engine is signed in.

---

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
| `settings.changed` | `{"settings": {Settings}}` | The person whose settings changed (everyone, each with their own, when the policy or voice availability changed) |
| `harness.status` | `{"harness": {HarnessStatus}}` (`path` always `null`) | All users |
| `message.updated` | `{"conversation_id", "message": {Message}}` | Conversation members |
| `models.changed` | `{"slots": [{Slot}]}` | All users |
| `harness.login` | `{"login": {LoginStatus}}` | The owner |
| `engines.changed` | `{"engines": [...]}` (as in `GET /api/engines`) | All users |

Example:

```json
{"type": "message.delta", "conversation_id": "c0ffee...", "message_id": "a1b2...", "text": "Sure, ", "at": 1760000000123}
```

Event details:

- **`message.updated`**: a stored message changed after it was created; so
  far only server cards whose `state` moves (sign-in, setup, connector
  reconnect). Replace the message by id.
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
  [section 8](#8-the-iris-mcp-bridge)), and after a successful feed job
  (`["feed", "ideas"]`, to the person it ran for).
- **`conversation.created`**: sent by `POST /api/conversations`,
  `POST /api/feed/{id}/discuss` (only when a new discussion is created),
  `POST /api/ideas/{id}/start`, and to the added user by
  `POST /api/conversations/{id}/members`. Removing a member sends no event.

Scheduled jobs run in hidden `job` conversations, and their turns emit no
`turn.status`, `message.*`, or card events at all. A job only becomes
visible through its results: `message.created` for a heartbeat post in the
Main chat, and `entities.changed` for data it changed.

### Client frames

| Frame | Effect |
|---|---|
| `{"type": "auth", "token": "..."}` | Only meaningful as the first frame. |
| `{"type": "ping"}` | Server replies `{"type": "pong"}`. |

Any other client frame is ignored.

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
3. It finds whose Iris answers: the conversation's creator (in a Together
   chat, the person who started it). Their persona, memory, name, and
   folders are used. It classifies the message (effort, and whether it is
   clearly hard), picks a slot and model ([section 7](#7-engine-selection)),
   applying the individual-use policy when the turn is not the owner's own
   use, and records it on the
   placeholder (`harness`, `model`, `effort`, and `meta.routing`; no event is
   sent for this). Images are left out with a conversational note when the
   chosen model cannot see them, and the bridge is skipped for a CLI without
   MCP support.
4. It resumes the CLI session for this conversation and harness when possible,
   replaying messages the session has not seen (up to 40 messages / 24,000
   characters), wires in the [iris-mcp bridge](#8-the-iris-mcp-bridge) and
   enabled connectors (only in the owner's personal chats; they are the
   owner's accounts), and starts the CLI in the person's `workspace/`, with
   their `memory/` writable. In shared conversations (more than one member)
   the bridge runs in cards-only mode, no connectors are passed, the CLI
   works in `<data dir>/together/<conversation id>`, and nothing is
   writable outside it. Claude Code also gets permission deny rules for
   every other person's folders and for Iris's own state (database, keys,
   per-turn files, uploads, library, voice cache), and in shared chats for
   the host's folders too; the other CLIs have no such rules and rely on the
   separate folders and the prompt.
5. While the CLI runs, Iris emits `message.delta` and `message.activity`.
6. On a "not signed in" error with no text yet, the harness is marked
   `auth_required` (a `harness.status` event, plus a reconnect card if the
   owner had used it) and the turn is retried on the next-ranked engine. On a failed session resume with no text yet, the turn is
   retried with a fresh session. At most 3 attempts in total.
7. Iris saves the final message and emits `message.completed`, then
   `message.created` for each card message the agent created during the turn
   (and a `connector_reconnect` card if a connector refused its sign-in),
   then `entities.changed` if applicable, then sends a reply push if
   applicable, and starts making the voice note if a member has voice
   replies on.

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
| `paused_policy` | Claude is the only usable engine, the turn is not the owner's own use (a conversation with more than one person, or another person's own Iris), and the owner has not allowed Claude for other people | `only Claude is available and this conversation is shared`, or `only Claude is available and this is not the owner's own Iris` | Group: "I can't reply in this group chat yet. The only AI engine on the server is Claude, and Claude subscriptions are for one person, so I stay out of shared conversations unless the owner turns that on." Another person's Iris: "I can't reply yet. The only AI engine on the server is Claude, and Claude subscriptions are for one person, so I can only use it for the owner unless they allow it. I've let them know." The first time per conversation, a group gets an `action` card with the owner's opt-in; another person's chat gets an `info` card, and the owner gets an `action` card ("Let Sam use Claude?") in their Main chat plus a push (`kind: "policy"`). |
| `no_engine` | No usable engine at all (none installed, all disabled, or every harness failed to start) | `no usable engine` / `no engine available` | "I can't reach any AI engine on the server right now. One of Claude, Codex, Gemini, or OpenCode needs to be installed and signed in; I can walk you through it in Settings." |
| `timeout` | No result within `turn_timeout_secs` (default 600) | `no result after 600s` | "That took too long, so I stopped. Want me to try again?" |
| `harness_error` | The CLI reported an error, or exited without a result | The CLI's error text | "Something went wrong on my end. Want me to try again?" |
| `internal` | Iris could not build the CLI command | `could not build command: ...` | "Something went wrong on my end. Want me to try again?" |

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

The `heartbeat` and `feed` jobs run for **every person's Iris**, as turns
with `purpose: "scheduled"` in hidden per-job conversations owned by that
person, on the `background` and `feed` slots (low effort), with their own
persona, memory, `HEARTBEAT.md`, feed prompt, and liked posts. The owner's
jobs are the owner's own use, so any signed-in engine may run them, Claude
included; other people's jobs follow the individual-use policy. Each run
starts a fresh CLI
session (no resume, no replay), and each job conversation is trimmed to its
last 20 messages before every run. Job turns are not streamed: no
`turn.status`, `message.*`, or card events are sent for them. The scheduler
checks every minute and runs a job when it is enabled, the local time is
inside `[active_start, active_end)` (windows may wrap past midnight; equal
start and end means all day), and at least `interval_mins` have passed since
that person's last start. Manual runs (`POST /api/jobs/{name}/run`,
`POST /api/feed/refresh`) skip those checks.

- **heartbeat**: the agent works through `HEARTBEAT.md`. If it replies
  `HEARTBEAT_OK` (or the reply contains it, or is empty), nothing happens.
  Otherwise the reply is posted as a new assistant message in that person's
  Main chat (`message.created`, `meta.initiated_by: "heartbeat"`) and pushed
  to all of their subscriptions.
- **feed**: the agent publishes 1 to 3 feed posts and up to 2 ideas through
  the bridge, guided by the person's feed prompt and recently liked posts.
  When the turn succeeds, that person receives `entities.changed` `["feed",
  "ideas"]`.

Failed job turns are only logged.

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
| `devices` | int | Only in `GET /api/users` for the owner: devices paired and not revoked. |

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
| `created_by` | string or null | The creator, whose Iris answers here (`null` once removed). |

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
| `kind` | string | `text` or `card`. |
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
| `type` | string | Agent cards: one of `link`, `connect`, `info`, `feed_post`, `idea`, `goal`, `result` (one concrete result such as a place or product: image, details, one action). Server cards (below): `action`, `engine_signin`, `engine_setup`, `connector_reconnect`, `approval`. |
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
| `engine_signin` | `harness`, `flow` (`code_paste`, `device_code`, `manual`), `state` (`needs_reconnect`, `starting`, `waiting_for_code`, `waiting_for_approval`, `verifying`, `connected`, `failed`, `cancelled`), `url`, `user_code`, `error`, `instructions` (manual flow), `login_id` | Start with `POST /api/harnesses/{harness}/login`. For `waiting_for_code`, show `url` and a code field (`/login/code`); for `waiting_for_approval`, show `url` and `user_code`; for `manual`, show `instructions` and a "Check again" button (the same `POST`). |
| `engine_setup` | `state` (`needs_setup`, `done`), `options: [{"harness", "label", "installed", "requires", "action": {"method", "path"}}]` | Each option installs that engine (`POST /api/engines/{name}/install`). |
| `connector_reconnect` | `connector_id`, `service`, `state` (`needs_reconnect`, `connected`) | Open the connector's settings so the owner can update its credentials (`PATCH /api/connectors/{id}`); the card turns `connected` when they do. |
| `approval` | `service`, `preview: {"to", "subject", "body"}`, `action_label` (e.g. "Send"), `approver_user_id`, `owner_only: false`, `state` (`pending`, `approved`, `denied`), `decided_at` | Show the preview exactly as given with Deny / `action_label`; decide with [`POST /api/messages/{id}/decision`](#post-apimessagesiddecision). The preview is built by Iris from the real action, never from agent text. |

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
| `why` | string or null | One line on why the user would like this post, max 300. |
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
| `name` | string | MCP server name. |
| `description` | string | |
| `transport` | string | `stdio` or `http`. |
| `enabled` | bool | |
| `command` | string or null | stdio. |
| `args` | string[] | stdio. |
| `url` | string or null | http. |
| `env_keys` | string[] | Names of stored env variables (values hidden). |
| `header_keys` | string[] | Names of stored headers (values hidden). |
| `created_at`, `updated_at` | int | |

```json
{"id": "...", "name": "calendar", "description": "", "transport": "stdio", "enabled": true,
 "command": "npx", "args": ["-y", "some-calendar-mcp"], "url": null,
 "env_keys": ["API_TOKEN"], "header_keys": [], "created_at": 1760000000000, "updated_at": 1760000000000}
```

### Credential

Metadata only; the encrypted secret is never returned.

| Field | Type | Notes |
|---|---|---|
| `id` | string | |
| `kind` | string | `login` or `card`. |
| `label` | string | |
| `metadata` | object | login: `{"site", "username"}`; card: `{"brand", "last4", "exp_month", "exp_year", "cardholder"}` (values may be `null`). |
| `created_at`, `updated_at` | int | |
| `last_used_at` | int or null | Always null for now. |

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
All fields except `allow_claude_for_shared` belong to the person's own Iris.

| Field | Type | Notes |
|---|---|---|
| `agent_name` | string | Setting, or `agent_name` from config (default `Iris`). |
| `avatar_url` | string | `/api/avatar?user=<user id>&v=<n>`; `v` changes when the avatar does (`default` when none is set). |
| `avatar_custom` | bool | A custom avatar is set. |
| `allow_claude_for_shared` | bool | Effective opt-in (config or app setting) for Claude answering other people: shared conversations and other people's own Iris. |
| `feed_prompt` | string | Effective feed prompt (default if unset). |
| `voice` | object | `{"enabled": bool, "tts": bool, "stt": bool}`: the person turned voice replies on; the server can speak (voice notes) and transcribe (voice input). |

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

Claude subscriptions are for one person: the owner, who signed the CLI in.
**Turns that are not the owner's own use never use Claude** unless the
owner opts in (`router.allow_claude_for_shared` in config, or
`allow_claude_for_shared` in `PUT /api/settings`). That covers
conversations with more than one member and everything another person's
own Iris does (their chats and their scheduled jobs). The owner's own chats
and scheduled jobs are ordinary individual use and may use Claude. "More
than one member" is literal: a group whose only member is its creator
counts as personal.

If Claude is the only usable engine, those turns pause: they fail with
`paused_policy`, and the owner is offered the opt-in once per conversation
(in a group, on a card there; for another person's Iris, on a card in the
owner's Main chat). Everything else keeps working. A pinned Claude slot
falls back to the next allowed model (`router: "fallback"`, reason
"individual-use policy").

People are also isolated from each other's data. A person's own Iris uses
only their persona, memory, and folders, and only the owner's personal
chats get the connectors. Shared conversations get no connectors, a
cards-only bridge (see [section 8](#8-the-iris-mcp-bridge)), a system
prompt with only the host Iris's `IDENTITY.md` and `SOUL.md` (no
`USER.md`, `TOOLS.md`, `PROACTIVE_PREFERENCES.md`, memory, or memory
index), their own working folder, and no write access to anyone's memory.
Claude Code is additionally denied reading or editing other people's
folders and Iris's own state. All people's CLIs run as the same operating
system account, so the other CLIs' read tools could still reach those
folders if a turn went looking; the prompt never names them.

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
| `codex` | `-c developer_instructions=...` | `-c mcp_servers.*` | `-c model_reasoning_effort` | `-i` | `exec resume` (new session when the persona changes) |
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
(reserved; connectors cannot use it), alongside every enabled connector
(none in shared conversations):

```
<path to iris binary> mcp --data-dir <DATA_DIR> --user <USER_ID> --conversation <CONVERSATION_ID> [--shared]
```

- `--user` is whose data the tools operate on: the conversation's creator for
  chat turns (so in a group conversation, its creator), the person a job
  runs for. Files saved to the Library must come from that person's own
  `workspace/` or `memory/`.
- `--conversation` is the conversation of the turn (cards are posted there,
  library items are linked to it). Without it, `create_card` fails.
- `--shared` is added for conversations with more than one member. In shared
  mode `tools/list` returns only `create_card`, and calling any other tool
  returns `isError: true` with `Error: <tool> is not available in shared
  conversations`. This keeps other members from reading or changing the
  creator's goals, ideas, feed, library, or connector list through the agent.
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

All tools are available in normal mode; only `create_card` in shared mode.

| Tool | Arguments | Result (`structuredContent`) |
|---|---|---|
| `list_goals` | `include_done?: bool` (default false) | `{"goals": [Goal]}` |
| `create_goal` | `title: string` (required, max 300), `status_text?: string` (max 2,000), `suggested?: bool` (a proposal the user confirms; default false) | `{"goal": Goal}` |
| `update_goal` | `id: string` (required), `title?` (non-blank, max 300), `status_text?` (max 2,000), `done?: bool` | `{"goal": Goal}`; error `no goal with that id` |
| `list_ideas` | `status?: "open" \| "started" \| "done" \| "dismissed"` | `{"ideas": [Idea]}` (max 200) |
| `create_idea` | `title` (required, max 300), `description?` (max 2,000), `prompt?` (max 4,000; sent when the user taps the idea) | `{"idea": Idea}` (status `open`) |
| `list_feed_posts` | `limit?: int` (1 to 50, default 10) | `{"posts": [FeedPost]}` |
| `create_feed_post` | `title` (required, max 300), `body` (required, Markdown, max 50,000), `topic?` (max 100), `source_url?`, `source_name?` (max 100), `image_url?` (http(s), max 2,000), `images?: string[]` (the page's original photos, http(s), max 10), `why?` (max 300) | `{"post": FeedPost}` |
| `save_to_library` | `title` (required, max 300), exactly one of `content` or `path`, `mime?`, `kind?: "artifact" \| "media"`, `description?` (max 2,000) | `{"item": LibraryItem}` |
| `list_library` | `kind?: "artifact" \| "media"` | `{"items": [LibraryItem]}` (max 500) |
| `create_card` | `type` (required, one of the [card types](#card)), `title` (required), `body?`, `url?`, `image_url?`, `action_label?`, `service?`, `ref_id?` (id of a related goal, idea, or feed post) | `{"card_message_id": "...", "shown": true}` |
| `list_connectors` | none | `{"connectors": [{"name", "description", "transport", "enabled"}]}` |

`create_card` validates with the [Card](#card) rules and inserts a card
message (`role: "assistant"`) into the current conversation. Clients receive
it as `message.created` right after the turn's `message.completed`.

### `save_to_library` details

- **`content`** (text, up to 1,000,000 characters): `mime` defaults to
  `text/markdown` and must be `text/*` or `application/json`. The file
  extension is `md`, `html`, `csv`, `json`, or `txt` based on `mime`.
- **`path`**: a relative path (resolved against the agent's workspace
  folder) or an absolute path. After resolving symlinks, the file must be
  inside the **workspace** or **memory** folder, must not itself be a
  symlink, must be a regular file, and must be at most 20 MiB. Otherwise:
  `only regular files in the working folder or memory folder can be saved`
  (or `file not
  found`, `file is larger than 20 MB`). `mime` defaults from the extension
  (`md`, `txt`, `csv`, `json`, `html`, `pdf`, `png`, `jpg`/`jpeg`, `gif`,
  `webp`, `svg`, `mp3`, `wav`, `mp4`; otherwise
  `application/octet-stream`).
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
encryption (RFC 8291). Push is enabled by default (`[push] enabled`); set
`vapid_subject` to a `mailto:` or `https:` URL. The VAPID key pair is
generated on first use.

### Subscribing (browser)

1. `GET /api/push/vapid-public-key` and pass `public_key` as
   `applicationServerKey` to `registration.pushManager.subscribe({userVisibleOnly: true, applicationServerKey})`.
2. `POST /api/push/subscriptions` with `subscription.toJSON()`.
3. Optionally `POST /api/push/test`.

### When pushes are sent

| Trigger | Recipients | `kind` |
|---|---|---|
| Heartbeat job posts a message in the Main chat | That person, always (online or not) | `heartbeat` |
| Someone sends a message in a Together chat | The other members with **no connected WebSocket** (title `<sender> · <chat title>`) | `together` |
| Another person's Iris is waiting for the owner's Claude opt-in | The owner | `policy` |
| A chat turn completes successfully (`status: "complete"`) and `push.notify_replies_when_offline` is true (default) | Conversation members with **no connected WebSocket** at that moment | `reply` |
| A chat turn fails (`status: "error"`), same setting | Same | `stuck` (body "I got stuck on that. Tap to try again.") |
| A CLI the owner used went signed out (reconnect card posted) | The owner | `engine_signin` |
| A connector refused its sign-in (reconnect card posted) | The owner | `connector_reconnect` |
| `POST /api/push/test` | The caller | `test` |

Cancelled and scheduled turns do not trigger reply pushes. Users inside
their [quiet hours](#put-apimequiet-hours) get no pushes except test pushes.

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
| `title` | The name of the Iris that replied (the host's in a Together chat), or the sender for `together`. |
| `body` | Message text cut to 180 characters (with `…` when cut). Test: `Notifications are working.` |
| `conversation_id`, `message_id` | Omitted in test pushes. |
| `kind` | `reply`, `stuck`, `heartbeat`, `together`, `policy`, `engine_signin`, `connector_reconnect`, or `test`. |

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

- **Approvals**: the `approval` card, its decision route, and the app's
  card are in place, but nothing creates approval cards yet. The plan is to
  route risky tool calls (sending email or messages, purchases, new
  recipients) through the CLI's own permission hook (for Claude Code,
  `--permission-prompt-tool` served by `iris mcp`), post the card from the
  real tool input, and wait for the decision.
- **Live view of Iris's computer**: there is no browser stream yet, so the
  app's "Iris's computer" screen shows recent activity and an honest empty
  state instead of Watch / Take over.
- **Connector sign-in (OAuth)**: connectors are configured as MCP servers
  with secrets; "Connect" in the app asks the agent to walk the user
  through it or opens the custom connector form.
- **Credential injection**: credentials can be stored and managed, but
  nothing gives them to the agent yet. `GET /api/credentials` returns
  `"injection": "not_implemented"` and `last_used_at` is always `null`.

---

## 11. The app

The Iris phone app is plain ES modules (Preact and htm vendored under
`web/vendor`, no build step) embedded in the binary and served at `/` with
a strict Content-Security-Policy (`script-src 'self'`). It is mobile first,
follows the phone's light or dark theme, and installs to the home screen
(`/manifest.webmanifest`). The service worker (`/sw.js`) caches the app
shell for offline start (never `/api`), shows push notifications, and opens
the right chat when one is tapped.

The app only uses the routes and events in this document. It stores its
device token in `localStorage` and fetches binary routes with the
`Authorization` header (object URLs). End users never see model names,
efforts, or tokens outside Settings, AI engine.

For development, `IRIS_WEB_DIR=web iris serve` serves the app files from
disk so changes need no rebuild. `iris demo-seed --assets
scripts/demo/images` fills a fresh data directory with fake demo content
(drawn illustrations, fake people and places) for screenshots.

