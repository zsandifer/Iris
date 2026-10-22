//! SQLite persistence (rusqlite, bundled). A single connection guarded by a
//! mutex and driven from Tokio's blocking pool is plenty for a personal
//! daemon and keeps memory low. The `iris mcp` bridge opens its own
//! connection to the same file; WAL mode keeps the two from blocking.
//!
//! Core tables live here; product entities (feed, ideas, goals, library,
//! connectors, credentials, push) are in `entities.rs`.

use crate::util::{Result, new_id, now_ms};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE users (
    id           TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    is_owner     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER,
    revoked_at   INTEGER
);

CREATE TABLE pairing_codes (
    code_hash  TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER
);

-- kind: main (one per user, persistent), side (topic chats, e.g. "Discuss"
-- on a feed post), group (several humans), job (hidden; scheduled work).
CREATE TABLE conversations (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL CHECK (kind IN ('main', 'side', 'group', 'job')),
    title       TEXT NOT NULL DEFAULT '',
    seed        TEXT,
    created_by  TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    archived_at INTEGER
);
CREATE UNIQUE INDEX conversations_one_main ON conversations(created_by) WHERE kind = 'main';

CREATE TABLE conversation_members (
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role            TEXT NOT NULL DEFAULT 'member',
    joined_at       INTEGER NOT NULL,
    last_read_at    INTEGER,
    PRIMARY KEY (conversation_id, user_id)
);
CREATE INDEX conversation_members_user ON conversation_members(user_id);

-- kind: text, or card (structured UI element in `card`).
-- harness/model/effort record the engine that produced an assistant message;
-- they are never shown to end users by default.
CREATE TABLE messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role            TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    author_user_id  TEXT REFERENCES users(id) ON DELETE SET NULL,
    kind            TEXT NOT NULL DEFAULT 'text' CHECK (kind IN ('text', 'card')),
    content         TEXT NOT NULL,
    attachments     TEXT NOT NULL DEFAULT '[]',
    card            TEXT,
    status          TEXT NOT NULL DEFAULT 'complete',
    harness         TEXT,
    model           TEXT,
    effort          TEXT,
    meta            TEXT NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX messages_conversation ON messages(conversation_id, created_at);

CREATE TABLE attachments (
    id         TEXT PRIMARY KEY,
    user_id    TEXT REFERENCES users(id) ON DELETE SET NULL,
    mime       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    sha256     TEXT NOT NULL,
    file_name  TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Harness session per conversation (Claude Code session_id, Codex thread_id)
-- for --resume continuity. synced_at is the created_at of the newest
-- conversation message the harness session has seen; anything newer is
-- replayed as context on the next turn (e.g. after switching harness).
-- persona_hash identifies the persona the session was started with; CLIs
-- that cannot update instructions on resume get a fresh session when it
-- changes.
CREATE TABLE harness_sessions (
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    harness         TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    synced_at       INTEGER NOT NULL DEFAULT 0,
    persona_hash    TEXT NOT NULL DEFAULT '',
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, harness)
);

CREATE TABLE feed_posts (
    id                      TEXT PRIMARY KEY,
    user_id                 TEXT REFERENCES users(id) ON DELETE CASCADE,
    title                   TEXT NOT NULL,
    body                    TEXT NOT NULL,
    topic                   TEXT,
    source_url              TEXT,
    image_url               TEXT,
    liked                   INTEGER NOT NULL DEFAULT 0,
    discuss_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);
CREATE INDEX feed_posts_recent ON feed_posts(created_at);

CREATE TABLE ideas (
    id              TEXT PRIMARY KEY,
    user_id         TEXT REFERENCES users(id) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    prompt          TEXT,
    status          TEXT NOT NULL DEFAULT 'open'
                    CHECK (status IN ('open', 'started', 'done', 'dismissed')),
    conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE goals (
    id          TEXT PRIMARY KEY,
    user_id     TEXT REFERENCES users(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    status_text TEXT NOT NULL DEFAULT '',
    done        INTEGER NOT NULL DEFAULT 0,
    done_at     INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- kind: artifact (documents, notes, code) or media (images, audio, video).
-- Content lives in library/<file_name>.
CREATE TABLE library_items (
    id              TEXT PRIMARY KEY,
    user_id         TEXT REFERENCES users(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK (kind IN ('artifact', 'media')),
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    mime            TEXT NOT NULL,
    file_name       TEXT NOT NULL,
    size            INTEGER NOT NULL,
    conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

-- MCP servers passed to the harness on every turn while enabled.
CREATE TABLE connectors (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    transport   TEXT NOT NULL CHECK (transport IN ('stdio', 'http')),
    config      TEXT NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Saved logins and wallet cards. `metadata` is non-secret display data;
-- `ciphertext` holds the encrypted secret and is never returned by the API.
CREATE TABLE credentials (
    id           TEXT PRIMARY KEY,
    user_id      TEXT REFERENCES users(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('login', 'card')),
    label        TEXT NOT NULL,
    metadata     TEXT NOT NULL DEFAULT '{}',
    ciphertext   BLOB NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_used_at INTEGER
);

CREATE TABLE push_subscriptions (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id       TEXT REFERENCES devices(id) ON DELETE CASCADE,
    endpoint        TEXT NOT NULL UNIQUE,
    p256dh          TEXT NOT NULL,
    auth            TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    last_success_at INTEGER,
    failure_count   INTEGER NOT NULL DEFAULT 0
);
"#,
    r#"
-- Feed posts carry the source page's original photos (first one is also
-- image_url), the source's display name, and a "why you'd like this" line.
ALTER TABLE feed_posts ADD COLUMN images TEXT NOT NULL DEFAULT '[]';
ALTER TABLE feed_posts ADD COLUMN source_name TEXT;
ALTER TABLE feed_posts ADD COLUMN why TEXT;
UPDATE feed_posts SET images = json_array(image_url) WHERE image_url IS NOT NULL;
CREATE INDEX feed_posts_liked ON feed_posts(user_id, liked);

-- Goals the agent suggested and the user has not confirmed yet.
ALTER TABLE goals ADD COLUMN suggested INTEGER NOT NULL DEFAULT 0;
"#,
    r#"
-- Message timestamps are unique across conversations (they are the
-- activity log's page cursor); this keeps finding the newest one cheap.
CREATE INDEX messages_created ON messages(created_at);
"#,
    r#"
-- Connector gateway: per-connector approval rules, catalog service, the
-- tripwire pause, and sign-in state for OAuth connectors.
ALTER TABLE connectors ADD COLUMN policy TEXT NOT NULL DEFAULT '{}';
ALTER TABLE connectors ADD COLUMN service TEXT NOT NULL DEFAULT 'custom';
ALTER TABLE connectors ADD COLUMN paused_reason TEXT;
ALTER TABLE connectors ADD COLUMN paused_at INTEGER;
ALTER TABLE connectors ADD COLUMN auth_state TEXT NOT NULL DEFAULT 'ok';
-- Connectors saved before the gateway keep running in the workspace (they
-- may use paths relative to it); new ones get a private folder.
ALTER TABLE connectors ADD COLUMN workspace_cwd INTEGER NOT NULL DEFAULT 1;

-- Every connector tool call that went through the gateway (activity log,
-- rate limits). `summary` is a short preview (recipients, subject).
CREATE TABLE connector_calls (
    id              TEXT PRIMARY KEY,
    connector_id    TEXT,
    connector_name  TEXT NOT NULL,
    tool            TEXT NOT NULL,
    category        TEXT NOT NULL,
    decision        TEXT NOT NULL,
    outcome         TEXT,
    reason          TEXT NOT NULL DEFAULT '',
    summary         TEXT NOT NULL DEFAULT '',
    conversation_id TEXT,
    message_id      TEXT,
    card_message_id TEXT,
    result_bytes    INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    admitted_at     INTEGER,
    finished_at     INTEGER
);
CREATE INDEX connector_calls_recent ON connector_calls(connector_id, created_at);
CREATE INDEX connector_calls_time ON connector_calls(created_at);
-- The tripwires' hourly usage (by when calls ran).
CREATE INDEX connector_calls_admitted ON connector_calls(connector_id, admitted_at);

-- Recipients the owner approved a message or invite to.
CREATE TABLE known_recipients (
    address    TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL
);

-- API keys for engines (Gemini CLI, OpenCode providers, optionally Claude
-- Code or Codex), sealed with the vault; passed to the CLI per turn.
CREATE TABLE engine_keys (
    harness    TEXT NOT NULL,
    env        TEXT NOT NULL,
    ciphertext TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (harness, env)
);
"#,
    r#"
-- Browser wallet approvals share the gateway cards and cancellation states.
CREATE INDEX messages_pending_approvals ON messages(conversation_id, created_at)
    WHERE kind = 'card' AND json_extract(card, '$.type') = 'approval'
      AND json_extract(card, '$.state') = 'pending';
"#,
    r#"
-- The For you pipeline (src/feed). Posts gain a story hook, the source's
-- favicon, local copies of their photos ({original url: cached file}),
-- the source kind, a canonical link key, and page facts (price, event).
ALTER TABLE feed_posts ADD COLUMN hook TEXT;
ALTER TABLE feed_posts ADD COLUMN favicon_url TEXT;
ALTER TABLE feed_posts ADD COLUMN cached TEXT NOT NULL DEFAULT '{}';
ALTER TABLE feed_posts ADD COLUMN source_kind TEXT;
ALTER TABLE feed_posts ADD COLUMN url_key TEXT;
ALTER TABLE feed_posts ADD COLUMN facts TEXT NOT NULL DEFAULT '{}';
CREATE INDEX feed_posts_user_recent ON feed_posts(user_id, created_at);

-- Where the pipeline looks. origin: user (added in the app), suggested (by
-- the daily interests call), google (YouTube subscriptions).
CREATE TABLE feed_sources (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL CHECK (kind IN ('rss', 'youtube', 'reddit', 'hn', 'bluesky', 'mastodon')),
    value         TEXT NOT NULL,
    label         TEXT,
    topic         TEXT,
    origin        TEXT NOT NULL DEFAULT 'user' CHECK (origin IN ('user', 'suggested', 'google')),
    enabled       INTEGER NOT NULL DEFAULT 1,
    -- Removed by the user: kept (hidden) so suggestions and imports do not
    -- bring it back; adding it again clears this.
    removed       INTEGER NOT NULL DEFAULT 0,
    -- The owner turned it on or off by hand: automatic trimming leaves it.
    decided       INTEGER NOT NULL DEFAULT 0,
    state         TEXT NOT NULL DEFAULT '{}',
    last_fetch_at INTEGER,
    last_ok_at    INTEGER,
    last_error    TEXT,
    fail_count    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    UNIQUE (user_id, kind, value)
);

-- Links and titles already shown, so nothing repeats.
CREATE TABLE feed_seen (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key     TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
);

-- Per local day: posts published, feed opens, and whether the user reached
-- the end of the day's posts (drives the adaptive daily size).
CREATE TABLE feed_days (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    day         TEXT NOT NULL,
    published   INTEGER NOT NULL DEFAULT 0,
    opened      INTEGER NOT NULL DEFAULT 0,
    reached_end INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, day)
);

-- Prices of saved (liked) products, checked daily for drops. A watch is
-- active while any liked post links to its URL (post_id is the first).
CREATE TABLE feed_watches (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    post_id     TEXT,
    url         TEXT NOT NULL,
    title       TEXT NOT NULL,
    price       REAL NOT NULL,
    currency    TEXT NOT NULL DEFAULT '',
    checked_at  INTEGER,
    created_at  INTEGER NOT NULL,
    UNIQUE (user_id, url)
);

-- Notifications held during quiet hours, delivered as a digest after.
CREATE TABLE push_queue (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    payload    TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX push_queue_user ON push_queue(user_id, created_at);
"#,
    r#"
-- A monotonic revision also prevents a running turn from restoring a
-- stale session, even if someone leaves and rejoins in one millisecond.
ALTER TABLE conversations ADD COLUMN membership_version INTEGER NOT NULL DEFAULT 0;
-- Existing shared sessions may already contain history hidden by joined_at.
DELETE FROM harness_sessions WHERE conversation_id IN (
    SELECT cm.conversation_id FROM conversation_members cm
    JOIN users u ON u.id = cm.user_id WHERE u.is_owner = 0
);
CREATE TRIGGER conversation_member_added AFTER INSERT ON conversation_members BEGIN
    DELETE FROM harness_sessions WHERE conversation_id = NEW.conversation_id;
    UPDATE conversations SET membership_version = membership_version + 1 WHERE id = NEW.conversation_id;
END;
CREATE TRIGGER conversation_member_removed AFTER DELETE ON conversation_members BEGIN
    DELETE FROM harness_sessions WHERE conversation_id = OLD.conversation_id;
    UPDATE conversations SET membership_version = membership_version + 1 WHERE id = OLD.conversation_id;
END;
"#,
    r#"
CREATE TABLE messages_new (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role            TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    author_user_id  TEXT REFERENCES users(id) ON DELETE SET NULL,
    kind            TEXT NOT NULL DEFAULT 'text' CHECK (kind IN ('text', 'card', 'system_note')),
    content         TEXT NOT NULL,
    attachments     TEXT NOT NULL DEFAULT '[]',
    card            TEXT,
    status          TEXT NOT NULL DEFAULT 'complete',
    harness         TEXT,
    model           TEXT,
    effort          TEXT,
    meta            TEXT NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
INSERT INTO messages_new SELECT * FROM messages;
DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;
CREATE INDEX messages_conversation ON messages(conversation_id, created_at);
CREATE INDEX messages_created ON messages(created_at);
CREATE INDEX messages_pending_approvals ON messages(conversation_id, created_at)
WHERE kind = 'card' AND json_extract(card, '$.type') = 'approval' AND json_extract(card, '$.state') = 'pending';
"#,
    r#"
-- The owner's long-term memory: one row per note (see memory/mod.rs).
-- Markdown in memory/ is a generated, read-only view of these rows.
CREATE TABLE memory_items (
    id            INTEGER PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('profile', 'preference', 'fact', 'event', 'person', 'project', 'routine')),
    topic         TEXT NOT NULL CHECK (topic IN ('you', 'people', 'projects', 'routines')),
    text          TEXT NOT NULL,
    importance    INTEGER NOT NULL DEFAULT 3 CHECK (importance BETWEEN 1 AND 5),
    pinned        INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    observed_at   INTEGER,
    mentioned_at  INTEGER,
    valid_to      INTEGER,
    superseded_by INTEGER REFERENCES memory_items(id) ON DELETE SET NULL,
    source        TEXT NOT NULL DEFAULT '{}',
    last_used_at  INTEGER,
    use_count     INTEGER NOT NULL DEFAULT 0,
    content_hash  TEXT NOT NULL,
    archived      INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL
);
-- A note is stored once among live rows; archived and superseded rows keep their text.
CREATE UNIQUE INDEX memory_items_live_hash ON memory_items(content_hash)
WHERE archived = 0 AND superseded_by IS NULL;
CREATE INDEX memory_items_live_topic ON memory_items(topic, created_at)
WHERE archived = 0 AND superseded_by IS NULL;
-- Finds set-aside notes by text too.
CREATE INDEX memory_items_hash ON memory_items(content_hash);

-- Every change, so any change can be undone. No foreign key: a tombstone
-- outlives the row it describes.
CREATE TABLE memory_history (
    id       INTEGER PRIMARY KEY,
    item_id  INTEGER NOT NULL,
    op       TEXT NOT NULL CHECK (op IN ('add', 'update', 'supersede', 'archive', 'restore', 'delete')),
    before   TEXT,
    after    TEXT,
    actor    TEXT NOT NULL CHECK (actor IN ('agent', 'dream', 'owner', 'import')),
    batch_id TEXT,
    at       INTEGER NOT NULL
);
CREATE INDEX memory_history_item ON memory_history(item_id, at);
CREATE INDEX memory_history_batch ON memory_history(batch_id) WHERE batch_id IS NOT NULL;

-- Keyword search over notes (external content: the text lives in memory_items).
CREATE VIRTUAL TABLE memory_items_fts USING fts5(
    text, content = 'memory_items', content_rowid = 'id', tokenize = 'porter unicode61 remove_diacritics 2'
);
CREATE TRIGGER memory_items_fts_insert AFTER INSERT ON memory_items BEGIN
    INSERT INTO memory_items_fts (rowid, text) VALUES (NEW.id, NEW.text);
END;
CREATE TRIGGER memory_items_fts_delete AFTER DELETE ON memory_items BEGIN
    INSERT INTO memory_items_fts (memory_items_fts, rowid, text) VALUES ('delete', OLD.id, OLD.text);
END;
CREATE TRIGGER memory_items_fts_update AFTER UPDATE OF text ON memory_items BEGIN
    INSERT INTO memory_items_fts (memory_items_fts, rowid, text) VALUES ('delete', OLD.id, OLD.text);
    INSERT INTO memory_items_fts (rowid, text) VALUES (NEW.id, NEW.text);
END;
"#,
    r#"
-- When the note last joined the always-in-prompt core (MEMORY.md); null
-- while it is not in it. Set when the export is written (memory/packing.rs).
ALTER TABLE memory_items ADD COLUMN core_since INTEGER;
-- Last turn that explicitly included this note in a MEMORY.md write.
-- Automatic packing must not make same-turn topic writes removable there.
ALTER TABLE memory_items ADD COLUMN core_written_batch TEXT;
"#,
    r#"
-- Supports the legacy file writer's stale hash checks after edits/deletes.
CREATE INDEX memory_history_before_hash_at ON memory_history(json_extract(before, '$.content_hash'), at);
-- Only owner-alone human chats are recallable. Once membership changes,
-- purge the conversation rather than reclassifying formerly shared text.
CREATE VIEW memory_private_messages AS
SELECT m.rowid AS message_rowid, m.id, m.conversation_id, m.role, m.content, m.created_at
FROM messages m JOIN conversations c ON c.id = m.conversation_id
JOIN users u ON u.id = c.created_by AND u.is_owner = 1
WHERE c.kind IN ('main', 'side') AND c.membership_version <= 1
AND m.kind = 'text' AND m.role IN ('user', 'assistant')
AND m.status = 'complete'
AND (m.role = 'assistant' OR m.author_user_id = u.id)
AND EXISTS (SELECT 1 FROM conversation_members cm WHERE cm.conversation_id = c.id AND cm.user_id = u.id)
AND NOT EXISTS (SELECT 1 FROM conversation_members cm WHERE cm.conversation_id = c.id AND cm.user_id != u.id);
CREATE VIRTUAL TABLE conversation_messages_fts USING fts5(
    message_id UNINDEXED, conversation_id UNINDEXED, text,
    tokenize = 'porter unicode61 remove_diacritics 2'
);
INSERT INTO conversation_messages_fts(rowid, message_id, conversation_id, text)
SELECT message_rowid, id, conversation_id, content FROM memory_private_messages;
CREATE TRIGGER conversation_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO conversation_messages_fts(rowid, message_id, conversation_id, text)
    SELECT message_rowid, id, conversation_id, content FROM memory_private_messages WHERE id = NEW.id;
END;
CREATE TRIGGER conversation_fts_update AFTER UPDATE OF content, status, kind, role, author_user_id, conversation_id ON messages BEGIN
    DELETE FROM conversation_messages_fts WHERE rowid = OLD.rowid;
    INSERT INTO conversation_messages_fts(rowid, message_id, conversation_id, text)
    SELECT message_rowid, id, conversation_id, content FROM memory_private_messages WHERE id = NEW.id;
END;
CREATE TRIGGER conversation_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM conversation_messages_fts WHERE rowid = OLD.rowid;
END;
CREATE TRIGGER conversation_fts_member_added AFTER INSERT ON conversation_members BEGIN
    DELETE FROM conversation_messages_fts WHERE conversation_id = NEW.conversation_id;
END;
CREATE TRIGGER conversation_fts_member_removed AFTER DELETE ON conversation_members BEGIN
    DELETE FROM conversation_messages_fts WHERE conversation_id = OLD.conversation_id;
END;
CREATE TRIGGER conversation_fts_scope_changed AFTER UPDATE OF kind, created_by, membership_version ON conversations BEGIN
    DELETE FROM conversation_messages_fts WHERE conversation_id = NEW.id;
END;
CREATE TRIGGER conversation_fts_owner_changed AFTER UPDATE OF is_owner ON users BEGIN
    DELETE FROM conversation_messages_fts WHERE conversation_id IN
        (SELECT id FROM conversations WHERE created_by = NEW.id);
END;
"#,
    r#"
-- Preserve identities after Forget everything clears both tables. Old UI
-- links and active CLI turns must never resolve to newly created notes.
INSERT INTO settings(key,value,updated_at) VALUES ('memory.item_highwater',
    (SELECT CAST(MAX(id) AS TEXT) FROM (SELECT COALESCE(MAX(id),0) AS id FROM memory_items UNION ALL SELECT COALESCE(MAX(item_id),0) FROM memory_history)), 0);
INSERT INTO settings(key,value,updated_at) VALUES ('memory.history_highwater',
    (SELECT CAST(COALESCE(MAX(id),0) AS TEXT) FROM memory_history), 0);
CREATE TRIGGER memory_item_identity AFTER INSERT ON memory_items BEGIN
    UPDATE settings SET value = CAST(MAX(CAST(value AS INTEGER), NEW.id) AS TEXT)
    WHERE key = 'memory.item_highwater';
END;
CREATE TRIGGER memory_history_identity AFTER INSERT ON memory_history BEGIN
    UPDATE settings SET value = CAST(MAX(CAST(value AS INTEGER), NEW.id) AS TEXT)
    WHERE key = 'memory.history_highwater';
END;
-- SQLite must find child rows without scanning the entire store per deletion.
CREATE INDEX memory_supersession ON memory_items(superseded_by);
CREATE INDEX memory_expiry ON memory_items(valid_to, id)
WHERE archived = 0 AND superseded_by IS NULL AND valid_to IS NOT NULL;
"#,
    r#"
-- Routines: scheduled jobs the owner (or Iris, with the owner's tap) set up
-- (routines/mod.rs). Times in `run_at`, `starts_at` and `end_at` are local
-- to `tz`; `next_run_at` is UTC milliseconds.
CREATE TABLE routines (
    id                    TEXT PRIMARY KEY,
    owner                 TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title                 TEXT NOT NULL,
    emoji                 TEXT NOT NULL DEFAULT '',
    instructions          TEXT NOT NULL,
    schedule_kind         TEXT NOT NULL CHECK (schedule_kind IN ('once', 'rule', 'cron')),
    run_at                TEXT,
    rule                  TEXT,
    cron_expr             TEXT,
    tz                    TEXT NOT NULL,
    starts_at             TEXT,
    end_at                TEXT,
    max_runs              INTEGER,
    run_count             INTEGER NOT NULL DEFAULT 0,
    state                 TEXT NOT NULL CHECK (state IN ('pending_approval', 'active', 'paused', 'done')),
    next_run_at           INTEGER,
    ask_before_acting     INTEGER NOT NULL DEFAULT 1,
    notify_when_done      INTEGER NOT NULL DEFAULT 1,
    approval_timeout_mins INTEGER NOT NULL DEFAULT 120,
    misfire               TEXT NOT NULL DEFAULT 'run_latest_in_window'
                          CHECK (misfire IN ('skip', 'run_latest_in_window')),
    max_retries           INTEGER NOT NULL DEFAULT 2,
    created_by            TEXT NOT NULL DEFAULT 'owner' CHECK (created_by IN ('owner', 'iris')),
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL
);
CREATE INDEX routines_due ON routines(next_run_at) WHERE state = 'active';

-- One row per scheduled slot (or manual run). `waiting`: prepared, waiting
-- for the owner's OK on an approval card until `expires_at`. A failed run
-- with `retry_at` is tried again then.
CREATE TABLE routine_runs (
    id              TEXT PRIMARY KEY,
    routine_id      TEXT NOT NULL REFERENCES routines(id) ON DELETE CASCADE,
    scheduled_for   INTEGER NOT NULL,
    manual          INTEGER NOT NULL DEFAULT 0,
    status          TEXT NOT NULL CHECK (status IN ('running', 'waiting', 'ok', 'skipped', 'failed')),
    reason          TEXT NOT NULL DEFAULT '',
    summary         TEXT NOT NULL DEFAULT '',
    attempts        INTEGER NOT NULL DEFAULT 0,
    -- The current attempt was allowed to act (a routine that does not ask
    -- first, as it was when the attempt started).
    may_act         INTEGER NOT NULL DEFAULT 0,
    retry_at        INTEGER,
    conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    card_message_id TEXT,
    expires_at      INTEGER,
    approved_at     INTEGER,
    started_at      INTEGER,
    finished_at     INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (routine_id, scheduled_for)
);
CREATE INDEX routine_runs_recent ON routine_runs(routine_id, scheduled_for);
CREATE INDEX routine_runs_open ON routine_runs(status) WHERE status IN ('running', 'waiting');
CREATE INDEX routine_runs_retry ON routine_runs(retry_at) WHERE retry_at IS NOT NULL;

-- Reference documents: Library items whose text goes into every run.
CREATE TABLE routine_docs (
    routine_id      TEXT NOT NULL REFERENCES routines(id) ON DELETE CASCADE,
    library_item_id TEXT NOT NULL REFERENCES library_items(id) ON DELETE CASCADE,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (routine_id, library_item_id)
);

-- Changes Iris proposed (create, update, delete). They wait for the
-- owner's tap on their chat card; `patch` holds the proposed fields.
CREATE TABLE routine_changes (
    id              TEXT PRIMARY KEY,
    routine_id      TEXT NOT NULL REFERENCES routines(id) ON DELETE CASCADE,
    action          TEXT NOT NULL CHECK (action IN ('create', 'update', 'delete')),
    patch           TEXT NOT NULL DEFAULT '{}',
    summary         TEXT NOT NULL,
    state           TEXT NOT NULL DEFAULT 'pending'
                    CHECK (state IN ('pending', 'approved', 'declined', 'superseded')),
    created_at      INTEGER NOT NULL,
    decided_at      INTEGER
);
CREATE INDEX routine_changes_pending ON routine_changes(routine_id) WHERE state = 'pending';

-- Text extracted once when a document is uploaded (routine documents).
ALTER TABLE library_items ADD COLUMN extracted_text TEXT;
"#,
    r#"
-- What done looks like, and when: the plan a check-in measures against.
ALTER TABLE goals ADD COLUMN target TEXT NOT NULL DEFAULT '';
ALTER TABLE goals ADD COLUMN due TEXT;

-- The agent's own working notes: one per domain it looks after (its plan,
-- the current state, what it is watching). Owner memory is about the
-- owner; these are the agent's picture of its work.
CREATE TABLE working_notes (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    domain      TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL DEFAULT '',
    updated_by  TEXT NOT NULL DEFAULT 'agent',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (user_id, domain)
);
-- Earlier versions of a note (capped per note), so a bad update can be read back.
CREATE TABLE working_note_history (
    id          INTEGER PRIMARY KEY,
    note_id     TEXT NOT NULL REFERENCES working_notes(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    updated_by  TEXT NOT NULL,
    replaced_at INTEGER NOT NULL
);
CREATE INDEX working_note_history_note ON working_note_history(note_id, replaced_at);

-- Every message the agent sent (or held back) on its own initiative, and
-- what the owner did with it. The scorecard the next check-in reads.
CREATE TABLE proactive_messages (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    message_id  TEXT,
    source      TEXT NOT NULL,
    excerpt     TEXT NOT NULL,
    outcome     TEXT NOT NULL DEFAULT 'posted'
                CHECK (outcome IN ('posted', 'held', 'useful', 'not_needed')),
    created_at  INTEGER NOT NULL,
    outcome_at  INTEGER
);
CREATE INDEX proactive_messages_recent ON proactive_messages(user_id, created_at);
CREATE INDEX proactive_messages_message ON proactive_messages(message_id);
"#,
];

/// Run `f` in a write transaction taken up front (`BEGIN IMMEDIATE`), so a
/// read-then-insert cannot interleave with another connection (the server
/// and `iris mcp` both write). Inside a caller's transaction, `f` just runs.
pub fn write_tx<R>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> rusqlite::Result<R>,
) -> rusqlite::Result<R> {
    if !conn.is_autocommit() {
        return f(conn);
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let out = f(&tx)?;
    tx.commit()?;
    Ok(out)
}

/// A creation time for a new row of `table`: now, or just after the newest
/// existing row, so timestamps stay unique and usable as page cursors even
/// within one millisecond. Call inside [`write_tx`].
pub fn next_timestamp(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    let last: Option<i64> =
        conn.query_row(&format!("SELECT MAX(created_at) FROM {table}"), [], |r| {
            r.get(0)
        })?;
    Ok(now_ms().max(last.map_or(0, |l| l + 1)))
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Keep the page cache small; this is a low-traffic personal database.
        conn.pragma_update(None, "cache_size", -1024)?;
        migrate(&mut conn)?;
        let owner = ensure_owner(&conn)?;
        ensure_main_conversation(&conn, &owner)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `f` against the connection on the blocking pool.
    pub async fn call<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        let out = tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&guard)
        })
        .await??;
        Ok(out)
    }

    /// Synchronous access for CLI commands and the MCP bridge.
    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> Result<R> {
        let guard = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        Ok(f(&guard)?)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version as usize > MIGRATIONS.len() {
        return Err(format!(
            "database schema version {version} is newer than this binary supports ({})",
            MIGRATIONS.len()
        )
        .into());
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        if i + 1 == UNIQUE_TIMESTAMPS_VERSION {
            spread_message_timestamps(&tx)?;
            spread_timestamps(&tx, "feed_posts")?;
        }
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// Schema version from which message and feed timestamps are unique.
const UNIQUE_TIMESTAMPS_VERSION: usize = 3;

/// Spread message timestamps, moving each harness session's watermark with
/// the newest message it had seen, so nothing it already has is replayed.
fn spread_message_timestamps(conn: &Connection) -> rusqlite::Result<()> {
    let sessions: Vec<(String, String, i64)> = conn
        .prepare("SELECT conversation_id, harness, synced_at FROM harness_sessions")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut anchors = Vec::new();
    for (conv, harness, synced) in sessions {
        let anchor: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM messages WHERE conversation_id = ?1 AND created_at <= ?2
                 ORDER BY created_at DESC, rowid DESC LIMIT 1",
                params![conv, synced],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(rowid) = anchor {
            anchors.push((conv, harness, rowid));
        }
    }
    spread_timestamps(conn, "messages")?;
    for (conv, harness, rowid) in anchors {
        conn.execute(
            "UPDATE harness_sessions SET synced_at = (SELECT created_at FROM messages WHERE rowid = ?3)
             WHERE conversation_id = ?1 AND harness = ?2",
            params![conv, harness, rowid],
        )?;
    }
    Ok(())
}

/// Make `created_at` unique across `table`: rows written in the same
/// millisecond before timestamps were allocated uniquely move forward a
/// millisecond at a time, keeping their order, so timestamp page cursors
/// are exact for old rows too.
fn spread_timestamps(conn: &Connection, table: &str) -> rusqlite::Result<()> {
    let rows: Vec<(i64, i64)> = conn
        .prepare(&format!(
            "SELECT rowid, created_at FROM {table} ORDER BY created_at, rowid"
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut update = conn.prepare(&format!(
        "UPDATE {table} SET created_at = ?2 WHERE rowid = ?1"
    ))?;
    let mut last = i64::MIN;
    for (rowid, at) in rows {
        let next = at.max(last.saturating_add(1));
        if next != at {
            update.execute(params![rowid, next])?;
        }
        last = next;
    }
    Ok(())
}

fn ensure_owner(conn: &Connection) -> rusqlite::Result<String> {
    if let Some(id) = owner_user_id(conn)? {
        return Ok(id);
    }
    let id = new_id();
    conn.execute(
        "INSERT INTO users (id, display_name, is_owner, created_at) VALUES (?1, 'Owner', 1, ?2)",
        params![id, now_ms()],
    )?;
    Ok(id)
}

pub fn owner_user_id(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT id FROM users WHERE is_owner = 1 ORDER BY created_at LIMIT 1",
        [],
        |r| r.get(0),
    )
    .optional()
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: String,
    pub display_name: String,
    pub is_owner: bool,
    pub created_at: i64,
}

pub fn get_user(conn: &Connection, id: &str) -> rusqlite::Result<Option<User>> {
    conn.query_row(
        "SELECT id, display_name, is_owner, created_at FROM users WHERE id = ?1",
        [id],
        |r| {
            Ok(User {
                id: r.get(0)?,
                display_name: r.get(1)?,
                is_owner: r.get(2)?,
                created_at: r.get(3)?,
            })
        },
    )
    .optional()
}

pub fn rename_user(conn: &Connection, id: &str, name: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE users SET display_name = ?2 WHERE id = ?1",
        params![id, name],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- settings

pub fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .optional()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now_ms()],
    )?;
    Ok(())
}

pub fn delete_setting(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
    Ok(())
}

// ------------------------------------------------------- pairing & devices

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

fn device_from_row(r: &rusqlite::Row) -> rusqlite::Result<Device> {
    Ok(Device {
        id: r.get("id")?,
        user_id: r.get("user_id")?,
        name: r.get("name")?,
        created_at: r.get("created_at")?,
        last_seen_at: r.get("last_seen_at")?,
        revoked_at: r.get("revoked_at")?,
    })
}

pub fn insert_pairing_code(
    conn: &Connection,
    code_hash: &str,
    user_id: &str,
    ttl_ms: i64,
) -> rusqlite::Result<()> {
    let now = now_ms();
    // Housekeeping: drop expired or used codes.
    conn.execute(
        "DELETE FROM pairing_codes WHERE expires_at < ?1 OR used_at IS NOT NULL",
        [now],
    )?;
    conn.execute(
        "INSERT INTO pairing_codes (code_hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![code_hash, user_id, now, now + ttl_ms],
    )?;
    Ok(())
}

/// Atomically mark a pairing code used. Returns the user it pairs to.
pub fn consume_pairing_code(
    conn: &Connection,
    code_hash: &str,
) -> rusqlite::Result<Option<String>> {
    let now = now_ms();
    conn.query_row(
        "UPDATE pairing_codes SET used_at = ?2
         WHERE code_hash = ?1 AND used_at IS NULL AND expires_at >= ?2
         RETURNING user_id",
        params![code_hash, now],
        |r| r.get(0),
    )
    .optional()
}

pub fn insert_device(
    conn: &Connection,
    user_id: &str,
    name: &str,
    token_hash: &str,
) -> rusqlite::Result<Device> {
    let id = new_id();
    conn.execute(
        "INSERT INTO devices (id, user_id, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, user_id, name, token_hash, now_ms()],
    )?;
    conn.query_row(
        "SELECT * FROM devices WHERE id = ?1",
        [&id],
        device_from_row,
    )
}

pub fn device_by_token_hash(
    conn: &Connection,
    token_hash: &str,
) -> rusqlite::Result<Option<Device>> {
    conn.query_row(
        "SELECT * FROM devices WHERE token_hash = ?1 AND revoked_at IS NULL",
        [token_hash],
        device_from_row,
    )
    .optional()
}

/// Update last_seen_at at most once a minute to avoid a write per request.
pub fn touch_device(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "UPDATE devices SET last_seen_at = ?2
         WHERE id = ?1 AND (last_seen_at IS NULL OR last_seen_at < ?2 - 60000)",
        params![id, now],
    )?;
    Ok(())
}

pub fn list_devices(conn: &Connection, user_id: &str) -> rusqlite::Result<Vec<Device>> {
    let mut stmt = conn.prepare("SELECT * FROM devices WHERE user_id = ?1 ORDER BY created_at")?;
    stmt.query_map([user_id], device_from_row)?.collect()
}

pub fn revoke_device(conn: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE devices SET revoked_at = ?3 WHERE id = ?1 AND user_id = ?2 AND revoked_at IS NULL",
        params![id, user_id, now_ms()],
    )?;
    if n > 0 {
        conn.execute("DELETE FROM push_subscriptions WHERE device_id = ?1", [id])?;
    }
    Ok(n > 0)
}

// ------------------------------------------------------------ conversations

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: String,
    /// main | side | group | job
    pub kind: String,
    pub title: String,
    /// What started a side chat, e.g. {"type":"feed_post","id":"..."}.
    pub seed: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
    pub member_count: i64,
}

fn conversation_from_row(r: &rusqlite::Row) -> rusqlite::Result<Conversation> {
    let seed: Option<String> = r.get("seed")?;
    Ok(Conversation {
        id: r.get("id")?,
        kind: r.get("kind")?,
        title: r.get("title")?,
        seed: seed.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        archived_at: r.get("archived_at")?,
        member_count: r.get("member_count")?,
    })
}

const CONVERSATION_SELECT: &str = "SELECT c.*,
    (SELECT COUNT(*) FROM conversation_members x WHERE x.conversation_id = c.id) AS member_count
    FROM conversations c";

pub fn create_conversation(
    conn: &Connection,
    user_id: &str,
    kind: &str,
    title: &str,
    seed: Option<&Value>,
) -> rusqlite::Result<Conversation> {
    let id = new_id();
    let now = now_ms();
    conn.execute(
        "INSERT INTO conversations (id, kind, title, seed, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, kind, title, seed.map(|s| s.to_string()), user_id, now],
    )?;
    conn.execute(
        "INSERT INTO conversation_members (conversation_id, user_id, role, joined_at)
         VALUES (?1, ?2, 'owner', ?3)",
        params![id, user_id, now],
    )?;
    get_conversation_any(conn, &id).map(|c| c.expect("just inserted"))
}

/// The user's persistent Main chat, created on demand.
pub fn ensure_main_conversation(conn: &Connection, user_id: &str) -> rusqlite::Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM conversations WHERE kind = 'main' AND created_by = ?1",
            [user_id],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => Ok(id),
        None => Ok(create_conversation(conn, user_id, "main", "Main chat", None)?.id),
    }
}

/// Hidden conversation for a scheduled job (heartbeat, feed).
pub fn ensure_job_conversation(
    conn: &Connection,
    user_id: &str,
    job: &str,
) -> rusqlite::Result<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM conversations WHERE kind = 'job' AND title = ?1 AND created_by = ?2",
            params![job, user_id],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => Ok(id),
        None => Ok(create_conversation(conn, user_id, "job", job, None)?.id),
    }
}

/// Conversations visible to the user (jobs are hidden), Main chat first.
pub fn list_conversations(
    conn: &Connection,
    user_id: &str,
    include_archived: bool,
) -> rusqlite::Result<Vec<Conversation>> {
    let sql = format!(
        "{CONVERSATION_SELECT}
         JOIN conversation_members m ON m.conversation_id = c.id
         WHERE m.user_id = ?1 AND c.kind != 'job' AND (?2 OR c.archived_at IS NULL)
         ORDER BY c.kind = 'main' DESC, c.updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(params![user_id, include_archived], conversation_from_row)?
        .collect()
}

/// Fetch a conversation only if `user_id` is a member.
pub fn get_conversation(
    conn: &Connection,
    user_id: &str,
    id: &str,
) -> rusqlite::Result<Option<Conversation>> {
    let sql = format!(
        "{CONVERSATION_SELECT}
         JOIN conversation_members m ON m.conversation_id = c.id
         WHERE c.id = ?1 AND m.user_id = ?2"
    );
    conn.query_row(&sql, params![id, user_id], conversation_from_row)
        .optional()
}

/// Fetch without a membership check (internal use only).
pub fn get_conversation_any(conn: &Connection, id: &str) -> rusqlite::Result<Option<Conversation>> {
    let sql = format!("{CONVERSATION_SELECT} WHERE c.id = ?1");
    conn.query_row(&sql, [id], conversation_from_row).optional()
}

pub fn update_conversation(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    archived: Option<bool>,
) -> rusqlite::Result<()> {
    let now = now_ms();
    if let Some(t) = title {
        conn.execute(
            "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, t, now],
        )?;
    }
    if let Some(a) = archived {
        conn.execute(
            "UPDATE conversations SET archived_at = ?2 WHERE id = ?1 AND kind != 'main'",
            params![id, a.then_some(now)],
        )?;
    }
    Ok(())
}

/// Delete a conversation. The Main chat cannot be deleted.
pub fn delete_conversation(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM conversations WHERE id = ?1 AND kind != 'main'",
        [id],
    )? > 0)
}

/// Joining must fall after all existing messages, including timestamps
/// advanced beyond wall time by the monotonic message cursor.
pub fn join_timestamp(conn: &Connection, conversation_id: &str) -> rusqlite::Result<i64> {
    conn.query_row("SELECT MAX(?2, COALESCE(MAX(created_at) + 1, ?2)) FROM messages WHERE conversation_id = ?1",
        params![conversation_id, now_ms()], |r| r.get(0))
}

pub fn conversation_member_ids(conn: &Connection, id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT user_id FROM conversation_members WHERE conversation_id = ?1")?;
    stmt.query_map([id], |r| r.get(0))?.collect()
}

pub fn mark_read(conn: &Connection, conversation_id: &str, user_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE conversation_members SET last_read_at = ?3 WHERE conversation_id = ?1 AND user_id = ?2",
        params![conversation_id, user_id, now_ms()],
    )?;
    Ok(())
}

// ----------------------------------------------------------------- messages

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub author_user_id: Option<String>,
    /// text | card
    pub kind: String,
    pub content: String,
    /// Attachment ids (see `GET /api/attachments/:id`).
    pub attachments: Vec<String>,
    pub card: Option<Value>,
    /// complete | streaming | error | cancelled | interrupted
    pub status: String,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Routing decision, tool activity, usage, error details.
    pub meta: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

fn message_from_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    let meta: String = r.get("meta")?;
    let attachments: String = r.get("attachments")?;
    let card: Option<String> = r.get("card")?;
    Ok(Message {
        id: r.get("id")?,
        conversation_id: r.get("conversation_id")?,
        role: r.get("role")?,
        author_user_id: r.get("author_user_id")?,
        kind: r.get("kind")?,
        content: r.get("content")?,
        attachments: serde_json::from_str(&attachments).unwrap_or_default(),
        card: card.and_then(|c| serde_json::from_str(&c).ok()),
        status: r.get("status")?,
        harness: r.get("harness")?,
        model: r.get("model")?,
        effort: r.get("effort")?,
        meta: serde_json::from_str(&meta).unwrap_or(Value::Null),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

#[derive(Default)]
pub struct NewMessage<'a> {
    pub conversation_id: &'a str,
    pub role: &'a str,
    pub author_user_id: Option<&'a str>,
    pub kind: &'a str,
    pub content: &'a str,
    pub attachments: &'a [String],
    pub card: Option<&'a Value>,
    pub status: &'a str,
    pub harness: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub meta: Option<&'a Value>,
}

pub fn insert_message(conn: &Connection, m: &NewMessage) -> rusqlite::Result<Message> {
    write_tx(conn, |conn| insert_message_locked(conn, m))
}

fn insert_message_locked(conn: &Connection, m: &NewMessage) -> rusqlite::Result<Message> {
    let id = new_id();
    // Strictly increasing, unique timestamps keep ordering stable and page
    // cursors exact even when two rows land in the same millisecond.
    let now = next_timestamp(conn, "messages")?;
    let kind = if m.kind.is_empty() { "text" } else { m.kind };
    let status = if m.status.is_empty() {
        "complete"
    } else {
        m.status
    };
    let meta = m.meta.cloned().unwrap_or_else(|| serde_json::json!({}));
    conn.execute(
        "INSERT INTO messages (id, conversation_id, role, author_user_id, kind, content,
                               attachments, card, status, harness, model, effort, meta,
                               created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
        params![
            id,
            m.conversation_id,
            m.role,
            m.author_user_id,
            kind,
            m.content,
            serde_json::to_string(m.attachments).unwrap_or_else(|_| "[]".into()),
            m.card.map(|c| c.to_string()),
            status,
            m.harness,
            m.model,
            m.effort,
            meta.to_string(),
            now
        ],
    )?;
    conn.execute(
        "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
        params![m.conversation_id, now],
    )?;
    get_message(conn, &id).map(|m| m.expect("just inserted"))
}

pub fn get_message(conn: &Connection, id: &str) -> rusqlite::Result<Option<Message>> {
    conn.query_row(
        "SELECT * FROM messages WHERE id = ?1",
        [id],
        message_from_row,
    )
    .optional()
}

/// Record the engine chosen for an assistant message once routing is done.
pub fn set_message_engine(
    conn: &Connection,
    id: &str,
    harness: &str,
    model: &str,
    effort: Option<&str>,
    meta: &Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE messages SET harness = ?2, model = ?3, effort = ?4, meta = ?5, updated_at = ?6
         WHERE id = ?1",
        params![id, harness, model, effort, meta.to_string(), now_ms()],
    )?;
    Ok(())
}

pub fn finish_message(
    conn: &Connection,
    id: &str,
    content: &str,
    status: &str,
    meta: &Value,
) -> rusqlite::Result<Message> {
    conn.execute(
        "UPDATE messages SET content = ?2, status = ?3, meta = ?4, updated_at = ?5 WHERE id = ?1",
        params![id, content, status, meta.to_string(), now_ms()],
    )?;
    get_message(conn, id).map(|m| m.expect("exists"))
}

/// Replace a card message's card (and its text fallback, the title).
pub fn update_message_card(conn: &Connection, id: &str, card: &Value) -> rusqlite::Result<Message> {
    let title = card["title"].as_str().unwrap_or("").to_string();
    conn.execute(
        "UPDATE messages SET card = ?2, content = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, card.to_string(), title, now_ms()],
    )?;
    get_message(conn, id).map(|m| m.expect("exists"))
}

/// Messages in chronological order. `before` (a created_at value) pages
/// backwards; at most `limit` of the newest matching messages are returned.
pub fn list_messages(
    conn: &Connection,
    conversation_id: &str,
    before: Option<i64>,
    limit: usize,
) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM (
             SELECT * FROM messages WHERE conversation_id = ?1 AND created_at < ?2
               AND status != 'pending'
             ORDER BY created_at DESC LIMIT ?3
         ) ORDER BY created_at",
    )?;
    stmt.query_map(
        params![conversation_id, before.unwrap_or(i64::MAX), limit as i64],
        message_from_row,
    )?
    .collect()
}

/// User-visible history, filtered in SQL before paging (cards are messages).
pub fn list_messages_for_member(
    conn: &Connection,
    user_id: &str,
    conversation_id: &str,
    before: Option<i64>,
    limit: usize,
) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare("SELECT * FROM (
        SELECT m.* FROM messages m JOIN conversation_members cm ON cm.conversation_id = m.conversation_id
        WHERE cm.user_id = ?1 AND m.conversation_id = ?2 AND m.created_at >= cm.joined_at
            AND m.created_at < ?3 AND m.status != 'pending'
        ORDER BY m.created_at DESC LIMIT ?4) ORDER BY created_at")?;
    stmt.query_map(
        params![
            user_id,
            conversation_id,
            before.unwrap_or(i64::MAX),
            limit as i64
        ],
        message_from_row,
    )?
    .collect()
}

/// Revalidate membership and its history boundary for a live/replayed event.
pub fn message_visible(
    conn: &Connection,
    user_id: &str,
    message_id: &str,
) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM messages m JOIN conversation_members cm
        ON cm.conversation_id = m.conversation_id WHERE cm.user_id = ?1 AND m.id = ?2 AND m.created_at >= cm.joined_at)", params![user_id, message_id], |r| r.get(0))
}

/// Messages created strictly after `after` and strictly before `before`.
pub fn messages_between(
    conn: &Connection,
    conversation_id: &str,
    after: i64,
    before: i64,
) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM messages WHERE conversation_id = ?1 AND created_at > ?2 AND created_at < ?3
         ORDER BY created_at",
    )?;
    stmt.query_map(params![conversation_id, after, before], message_from_row)?
        .collect()
}

/// The newest `limit` finished, non-empty messages strictly between `after`
/// and `before`, oldest first: the replay for a harness session. Limited in
/// SQL so a long unsynced history is never loaded whole. Shared context
/// starts at the latest join among current non-owner members.
pub fn replay_messages(
    conn: &Connection,
    conversation_id: &str,
    after: i64,
    before: i64,
    limit: usize,
) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM (
             SELECT * FROM messages
             WHERE conversation_id = ?1 AND created_at > ?2 AND created_at < ?3
               AND created_at >= COALESCE((
                   SELECT MAX(cm.joined_at) FROM conversation_members cm
                   JOIN users u ON u.id = cm.user_id
                   WHERE cm.conversation_id = ?1 AND u.is_owner = 0
               ), 0)
               AND status NOT IN ('streaming', 'pending') AND trim(content) != ''
             ORDER BY created_at DESC LIMIT ?4
         ) ORDER BY created_at",
    )?;
    stmt.query_map(
        params![conversation_id, after, before, limit as i64],
        message_from_row,
    )?
    .collect()
}

/// The newest message a harness session has seen once the turn whose reply
/// placeholder was created at `placeholder` is done: the placeholder, plus
/// the cards the agent created right after it, but never past the first
/// message the session did not see (a queued user message, a heartbeat
/// post), so that one is still replayed later.
pub fn synced_watermark(
    conn: &Connection,
    conversation_id: &str,
    placeholder: i64,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(created_at), ?2) FROM messages
         WHERE conversation_id = ?1 AND kind = 'card' AND created_at > ?2
           AND created_at < COALESCE(
               (SELECT MIN(created_at) FROM messages
                WHERE conversation_id = ?1 AND created_at > ?2 AND kind != 'card'),
               9223372036854775807)",
        params![conversation_id, placeholder],
        |r| r.get(0),
    )
}

/// Mark turns that were streaming when the daemon stopped.
/// Cards still held back by a private turn that never finished are dropped
/// (nothing decided whether they may be shown).
pub fn mark_interrupted(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM messages WHERE kind = 'card' AND status = 'pending'",
        [],
    )?;
    conn.execute(
        "UPDATE messages SET status = 'interrupted', updated_at = ?1 WHERE status = 'streaming'",
        [now_ms()],
    )
}

// -------------------------------------------------------------- attachments

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    pub id: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
    #[serde(skip)]
    pub file_name: String,
    #[serde(skip)]
    pub user_id: Option<String>,
    pub created_at: i64,
}

pub fn insert_attachment(
    conn: &Connection,
    id: &str,
    user_id: &str,
    mime: &str,
    size: i64,
    sha256: &str,
    file_name: &str,
) -> rusqlite::Result<Attachment> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO attachments (id, user_id, mime, size, sha256, file_name, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, user_id, mime, size, sha256, file_name, now],
    )?;
    Ok(Attachment {
        id: id.into(),
        mime: mime.into(),
        size,
        sha256: sha256.into(),
        file_name: file_name.into(),
        user_id: Some(user_id.into()),
        created_at: now,
    })
}

pub fn get_attachment(conn: &Connection, id: &str) -> rusqlite::Result<Option<Attachment>> {
    conn.query_row(
        "SELECT id, user_id, mime, size, sha256, file_name, created_at FROM attachments WHERE id = ?1",
        [id],
        |r| {
            Ok(Attachment {
                id: r.get(0)?,
                user_id: r.get(1)?,
                mime: r.get(2)?,
                size: r.get(3)?,
                sha256: r.get(4)?,
                file_name: r.get(5)?,
                created_at: r.get(6)?,
            })
        },
    )
    .optional()
}

/// A photo the user sent in a chat (for the Library).
#[derive(Debug, Clone, Serialize)]
pub struct SentPhoto {
    #[serde(flatten)]
    pub attachment: Attachment,
    pub conversation_id: String,
    pub message_id: String,
    /// When it was last sent; with `id`, the page cursor.
    pub used_at: i64,
}

/// Images the user attached to their own messages, newest first (each
/// attachment once, at its latest use). `before` is the last item of the
/// previous page as `(used_at, id)`: one message can carry several photos
/// with the same `used_at`, so the id breaks ties.
pub fn sent_photos(
    conn: &Connection,
    user_id: &str,
    before: Option<(i64, &str)>,
    limit: usize,
) -> rusqlite::Result<Vec<SentPhoto>> {
    let (before_at, before_id) = before.unwrap_or((i64::MAX, ""));
    let mut stmt = conn.prepare(
        "SELECT a.id, a.user_id, a.mime, a.size, a.sha256, a.file_name, a.created_at,
                m.conversation_id, m.id, MAX(m.created_at) AS used_at
         FROM messages m
         JOIN json_each(m.attachments) j
         JOIN attachments a ON a.id = j.value
         JOIN conversation_members cm ON cm.conversation_id = m.conversation_id AND cm.user_id = ?1
         WHERE m.author_user_id = ?1 AND m.attachments != '[]' AND m.created_at >= cm.joined_at
         GROUP BY a.id
         HAVING used_at < ?3 OR (used_at = ?3 AND (?4 = '' OR a.id < ?4))
         ORDER BY used_at DESC, a.id DESC
         LIMIT ?2",
    )?;
    stmt.query_map(params![user_id, limit as i64, before_at, before_id], |r| {
        Ok(SentPhoto {
            attachment: Attachment {
                id: r.get(0)?,
                user_id: r.get(1)?,
                mime: r.get(2)?,
                size: r.get(3)?,
                sha256: r.get(4)?,
                file_name: r.get(5)?,
                created_at: r.get(6)?,
            },
            conversation_id: r.get(7)?,
            message_id: r.get(8)?,
            used_at: r.get(9)?,
        })
    })?
    .collect()
}

/// One finished turn's tool activity, for the activity log.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub message_id: String,
    pub conversation_id: String,
    pub conversation_kind: String,
    pub conversation_title: String,
    pub status: String,
    pub activity: Value,
    pub duration_ms: Option<i64>,
    pub created_at: i64,
}

/// Recent assistant turns that used tools, in the user's conversations
/// (plus, for `include_jobs`, the hidden job conversations they own),
/// newest first. Only the activity list is returned, never routing or usage.
pub fn recent_activity(
    conn: &Connection,
    user_id: &str,
    include_jobs: bool,
    before: Option<i64>,
    limit: usize,
) -> rusqlite::Result<Vec<ActivityEntry>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.conversation_id, c.kind, c.title, m.status,
                json_extract(m.meta, '$.activity'), json_extract(m.meta, '$.duration_ms'), m.created_at
         FROM messages m JOIN conversations c ON c.id = m.conversation_id
         WHERE m.role = 'assistant' AND m.created_at < ?3
           AND json_array_length(m.meta, '$.activity') > 0
           AND (EXISTS (SELECT 1 FROM conversation_members cm WHERE cm.user_id = ?1 AND cm.conversation_id = m.conversation_id AND m.created_at >= cm.joined_at)
                OR (?2 AND c.kind = 'job' AND c.created_by = ?1))
           AND (c.kind != 'job' OR ?2)
         ORDER BY m.created_at DESC
         LIMIT ?4",
    )?;
    stmt.query_map(
        params![
            user_id,
            include_jobs,
            before.unwrap_or(i64::MAX),
            limit as i64
        ],
        |r| {
            let activity: Option<String> = r.get(5)?;
            Ok(ActivityEntry {
                message_id: r.get(0)?,
                conversation_id: r.get(1)?,
                conversation_kind: r.get(2)?,
                conversation_title: r.get(3)?,
                status: r.get(4)?,
                activity: activity
                    .and_then(|a| serde_json::from_str(&a).ok())
                    .unwrap_or(Value::Array(Vec::new())),
                duration_ms: r.get(6)?,
                created_at: r.get(7)?,
            })
        },
    )?
    .collect()
}

/// Monotonic revision for detecting membership changes during a CLI turn.
pub fn conversation_membership_version(
    conn: &Connection,
    conversation_id: &str,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT membership_version FROM conversations WHERE id = ?1",
        [conversation_id],
        |r| r.get(0),
    )
}

// --------------------------------------------------------- harness sessions

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSession {
    pub session_id: String,
    pub synced_at: i64,
    pub persona_hash: String,
}

pub fn get_harness_session(
    conn: &Connection,
    conversation_id: &str,
    harness: &str,
) -> rusqlite::Result<Option<HarnessSession>> {
    conn.query_row(
        "SELECT session_id, synced_at, persona_hash FROM harness_sessions
         WHERE conversation_id = ?1 AND harness = ?2",
        params![conversation_id, harness],
        |r| {
            Ok(HarnessSession {
                session_id: r.get(0)?,
                synced_at: r.get(1)?,
                persona_hash: r.get(2)?,
            })
        },
    )
    .optional()
}

pub fn set_harness_session(
    conn: &Connection,
    conversation_id: &str,
    harness: &str,
    s: &HarnessSession,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO harness_sessions (conversation_id, harness, session_id, synced_at, persona_hash, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(conversation_id, harness) DO UPDATE
         SET session_id = excluded.session_id, synced_at = excluded.synced_at,
             persona_hash = excluded.persona_hash, updated_at = excluded.updated_at",
        params![
            conversation_id,
            harness,
            s.session_id,
            s.synced_at,
            s.persona_hash,
            now_ms()
        ],
    )?;
    Ok(())
}

pub fn clear_harness_session(
    conn: &Connection,
    conversation_id: &str,
    harness: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM harness_sessions WHERE conversation_id = ?1 AND harness = ?2",
        params![conversation_id, harness],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text<'a>(conv: &'a str, owner: &'a str, content: &'a str) -> NewMessage<'a> {
        NewMessage {
            conversation_id: conv,
            role: "user",
            author_user_id: Some(owner),
            content,
            ..Default::default()
        }
    }

    #[test]
    fn conversation_recall_migration_backfills_only_owner_private_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("legacy.db");
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "foreign_keys", "ON").unwrap();
            for migration in &MIGRATIONS[..10] {
                c.execute_batch(migration).unwrap();
            }
            c.pragma_update(None, "user_version", 10).unwrap();
            let owner = ensure_owner(&c).unwrap();
            let private = ensure_main_conversation(&c, &owner).unwrap();
            let group = create_conversation(&c, &owner, "group", "Practice group", None)
                .unwrap()
                .id;
            c.execute(
                "INSERT INTO users(id,display_name,created_at) VALUES ('guest','Guest',0)",
                [],
            )
            .unwrap();
            c.execute("INSERT INTO conversation_members(conversation_id,user_id,joined_at) VALUES (?1,'guest',0)", [&group]).unwrap();
            let other = create_conversation(&c, "guest", "side", "Guest chat", None)
                .unwrap()
                .id;
            for chat in [&private, &group, &other] {
                insert_message(
                    &c,
                    &NewMessage {
                        conversation_id: chat,
                        role: "assistant",
                        content: "Synthetic orchard",
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            insert_message(
                &c,
                &NewMessage {
                    conversation_id: &private,
                    role: "assistant",
                    kind: "card",
                    content: "Orchard card",
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            let hits = crate::memory::recall::conversations(c, "orchard")?;
            assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
            let count: i64 =
                c.query_row("SELECT COUNT(*) FROM conversation_messages_fts", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(count, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn owner_and_main_chat_exist_after_open() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let convs = list_conversations(c, &owner, false)?;
            assert_eq!(convs.len(), 1);
            assert_eq!(convs[0].kind, "main");
            assert_eq!(ensure_main_conversation(c, &owner)?, convs[0].id);
            assert!(!delete_conversation(c, &convs[0].id)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn conversation_membership_scopes_access() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            c.execute(
                "INSERT INTO users (id, display_name, created_at) VALUES ('guest', 'Guest', 0)",
                [],
            )?;
            let conv = create_conversation(c, &owner, "side", "t", None)?;
            assert!(get_conversation(c, &owner, &conv.id)?.is_some());
            assert!(get_conversation(c, "guest", &conv.id)?.is_none());
            assert_eq!(list_conversations(c, "guest", false)?.len(), 0);
            assert_eq!(conversation_member_ids(c, &conv.id)?, vec![owner.clone()]);
            let job = ensure_job_conversation(c, &owner, "heartbeat")?;
            assert_eq!(ensure_job_conversation(c, &owner, "heartbeat")?, job);
            assert!(
                list_conversations(c, &owner, false)?
                    .iter()
                    .all(|x| x.kind != "job")
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn messages_keep_order_page_and_cascade() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let conv = create_conversation(c, &owner, "side", "", None)?;
            for t in ["a", "b", "c", "d"] {
                insert_message(c, &text(&conv.id, &owner, t))?;
            }
            let msgs = list_messages(c, &conv.id, None, 100)?;
            let texts: Vec<_> = msgs.iter().map(|m| m.content.as_str()).collect();
            assert_eq!(texts, ["a", "b", "c", "d"]);
            assert_eq!(msgs[0].kind, "text");
            assert_eq!(msgs[0].status, "complete");
            let page = list_messages(c, &conv.id, Some(msgs[3].created_at), 2)?;
            let texts: Vec<_> = page.iter().map(|m| m.content.as_str()).collect();
            assert_eq!(texts, ["b", "c"]);
            let mid = messages_between(c, &conv.id, msgs[0].created_at, msgs[3].created_at)?;
            assert_eq!(mid.len(), 2);
            let sess = |id: &str, synced| HarnessSession {
                session_id: id.into(),
                synced_at: synced,
                persona_hash: "h".into(),
            };
            set_harness_session(c, &conv.id, "claude", &sess("s1", 1))?;
            set_harness_session(c, &conv.id, "claude", &sess("s2", 2))?;
            assert_eq!(
                get_harness_session(c, &conv.id, "claude")?,
                Some(sess("s2", 2))
            );
            assert!(delete_conversation(c, &conv.id)?);
            assert!(list_messages(c, &conv.id, None, 10)?.is_empty());
            assert!(get_harness_session(c, &conv.id, "claude")?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn watermark_stops_at_the_first_unseen_message() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let conv = create_conversation(c, &owner, "side", "", None)?.id;
            let card = |c: &Connection| {
                crate::entities::insert_card(
                    c,
                    &conv,
                    &serde_json::json!({"type": "info", "title": "x"}),
                    "assistant",
                )
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
            };
            let placeholder = insert_message(c, &text(&conv, &owner, "reply"))?.created_at;
            assert_eq!(synced_watermark(c, &conv, placeholder)?, placeholder);
            let c1 = card(c)?.created_at;
            assert_eq!(
                synced_watermark(c, &conv, placeholder)?,
                c1,
                "the turn's card"
            );
            // A heartbeat post lands, then the turn creates another card.
            let heartbeat = insert_message(c, &text(&conv, &owner, "heartbeat"))?.created_at;
            card(c)?;
            assert_eq!(synced_watermark(c, &conv, placeholder)?, c1);
            assert!(c1 < heartbeat);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn system_note_migration_preserves_history_and_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        let (conv, text_id, card_id) = {
            let c = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..7] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 7).unwrap();
            let owner = ensure_owner(&c).unwrap();
            let conv = ensure_main_conversation(&c, &owner).unwrap();
            let message = insert_message(&c, &text(&conv, &owner, "Existing history")).unwrap();
            let card = crate::entities::insert_card(&c, &conv, &serde_json::json!({"type": "engine_signin", "title": "Sign in", "state": "needs_reconnect"}), "assistant").unwrap();
            (conv, message.id, card.id)
        };
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            assert_eq!(get_message(c, &text_id)?.unwrap().content, "Existing history");
            assert_eq!(get_message(c, &card_id)?.unwrap().card.unwrap()["state"], "needs_reconnect");
            crate::awareness::insert_note(c, &conv, "Engine signed in ✓")?;
            let indexes: i64 = c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages' AND name IN ('messages_conversation', 'messages_created', 'messages_pending_approvals')", [], |r| r.get(0))?;
            assert_eq!(indexes, 3);
            assert!(!c.prepare("PRAGMA foreign_key_check")?.exists([])?);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn memory_identity_migration_keeps_deleted_item_and_history_ids() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for migration in &MIGRATIONS[..11] {
            conn.execute_batch(migration).unwrap();
        }
        conn.execute(
            "INSERT INTO memory_history(id,item_id,op,actor,at) VALUES (77,42,'delete','owner',0)",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 11).unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(
            get_setting(&conn, "memory.item_highwater")
                .unwrap()
                .as_deref(),
            Some("42")
        );
        assert_eq!(
            get_setting(&conn, "memory.history_highwater")
                .unwrap()
                .as_deref(),
            Some("77")
        );
        let plan: Vec<String> = conn
            .prepare("EXPLAIN QUERY PLAN DELETE FROM memory_items")
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|line| line
                .contains("SEARCH memory_items USING COVERING INDEX memory_supersession")),
            "{plan:?}"
        );
        conn.execute("DELETE FROM memory_history", []).unwrap();
        let saved = crate::memory::recall::save(
            &conn,
            &crate::memory::recall::Edit {
                text: Some("New synthetic note".into()),
                ..Default::default()
            },
            None,
            crate::memory::Actor::Owner,
            &serde_json::json!({}),
            "new",
        )
        .unwrap();
        assert_eq!(saved["item"]["id"], 43);
        assert_eq!(saved["history_id"], 78);
    }

    #[test]
    fn memory_migration_adds_notes_history_and_search() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        let (conv, message) = {
            let c = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..8] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 8).unwrap();
            let owner = ensure_owner(&c).unwrap();
            let conv = ensure_main_conversation(&c, &owner).unwrap();
            let m = insert_message(&c, &text(&conv, &owner, "Before memory rows")).unwrap();
            (conv, m.id)
        };
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            assert_eq!(v as usize, MIGRATIONS.len());
            assert_eq!(get_message(c, &message)?.unwrap().conversation_id, conv);
            c.execute(
                "INSERT INTO memory_items (kind, topic, text, created_at, content_hash, updated_at)
                 VALUES ('person', 'people', 'Neighbour waters the plants', 1, 'h1', 1)",
                [],
            )?;
            let id = c.last_insert_rowid();
            let (importance, pinned, use_count, archived, source): (i64, bool, i64, bool, String) =
                c.query_row(
                    "SELECT importance, pinned, use_count, archived, source FROM memory_items WHERE id = ?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?;
            assert_eq!((importance, pinned, use_count, archived, source.as_str()), (3, false, 0, false, "{}"));
            let core_since: Option<i64> =
                c.query_row("SELECT core_since FROM memory_items WHERE id = ?1", [id], |r| r.get(0))?;
            assert_eq!(core_since, None, "not in the core until the export is written");
            let core_written_batch: Option<String> =
                c.query_row("SELECT core_written_batch FROM memory_items WHERE id = ?1", [id], |r| r.get(0))?;
            assert_eq!(core_written_batch, None, "not acknowledged by a core write yet");
            let found: i64 = c.query_row(
                "SELECT rowid FROM memory_items_fts WHERE memory_items_fts MATCH 'plant'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(found, id);
            // Kinds, topics, importance, ops and actors are checked.
            for bad in [
                "INSERT INTO memory_items (kind, topic, text, created_at, content_hash, updated_at) VALUES ('secret', 'you', 'x', 1, 'h2', 1)",
                "INSERT INTO memory_items (kind, topic, text, created_at, content_hash, updated_at) VALUES ('fact', 'work', 'x', 1, 'h2', 1)",
                "INSERT INTO memory_items (kind, topic, text, importance, created_at, content_hash, updated_at) VALUES ('fact', 'you', 'x', 6, 1, 'h2', 1)",
                "INSERT INTO memory_history (item_id, op, actor, at) VALUES (1, 'merge', 'agent', 1)",
                "INSERT INTO memory_history (item_id, op, actor, at) VALUES (1, 'add', 'guest', 1)",
            ] {
                assert!(c.execute(bad, []).is_err(), "{bad}");
            }
            for actor in ["agent", "dream", "owner", "import"] {
                c.execute(
                    "INSERT INTO memory_history (item_id, op, actor, at) VALUES (?1, 'add', ?2, 1)",
                    params![id, actor],
                )?;
            }
            // A superseded or archived note frees its text for a new live note.
            c.execute(
                "INSERT INTO memory_items (kind, topic, text, created_at, content_hash, updated_at)
                 VALUES ('fact', 'you', 'Neighbour waters the plants', 2, 'h1', 2)",
                [],
            )
            .unwrap_err();
            c.execute("UPDATE memory_items SET archived = 1 WHERE id = ?1", [id])?;
            c.execute(
                "INSERT INTO memory_items (kind, topic, text, created_at, content_hash, updated_at)
                 VALUES ('fact', 'you', 'Neighbour waters the plants', 2, 'h1', 2)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn membership_migration_discards_existing_shared_sessions_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        let (private, shared) = {
            let c = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..6] {
                c.execute_batch(sql).unwrap();
            }
            c.pragma_update(None, "user_version", 6).unwrap();
            let owner = ensure_owner(&c).unwrap();
            let private = create_conversation(&c, &owner, "side", "Private", None)
                .unwrap()
                .id;
            let shared = create_conversation(&c, &owner, "group", "Shared", None)
                .unwrap()
                .id;
            c.execute(
                "INSERT INTO users (id, display_name, created_at) VALUES ('guest', 'Guest', 0)",
                [],
            )
            .unwrap();
            c.execute("INSERT INTO conversation_members (conversation_id, user_id, joined_at) VALUES (?1, 'guest', 1)", [&shared]).unwrap();
            for conv in [&private, &shared] {
                set_harness_session(
                    &c,
                    conv,
                    "claude",
                    &HarnessSession {
                        session_id: "old-session".into(),
                        synced_at: 0,
                        persona_hash: "test".into(),
                    },
                )
                .unwrap();
            }
            (private, shared)
        };
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            assert!(get_harness_session(c, &private, "claude")?.is_some());
            assert!(get_harness_session(c, &shared, "claude")?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn replay_is_limited_in_sql() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let conv = create_conversation(c, &owner, "side", "", None)?.id;
            for i in 0..100 {
                insert_message(c, &text(&conv, &owner, &format!("m{i}")))?;
            }
            insert_message(c, &text(&conv, &owner, "   "))?;
            let got = replay_messages(c, &conv, 0, i64::MAX, 40)?;
            assert_eq!(got.len(), 40);
            assert_eq!(got[0].content, "m60");
            assert_eq!(got[39].content, "m99");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn timestamps_are_unique_across_connections() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        let first = Db::open(&path).unwrap();
        let (owner, convs) = first
            .with(|c| {
                let owner = owner_user_id(c)?.unwrap();
                let a = create_conversation(c, &owner, "side", "", None)?.id;
                let b = create_conversation(c, &owner, "side", "", None)?.id;
                Ok((owner, [a, b]))
            })
            .unwrap();
        let workers: Vec<_> = (0..4)
            .map(|w| {
                let (path, owner, conv) = (path.clone(), owner.clone(), convs[w % 2].clone());
                std::thread::spawn(move || {
                    let db = Db::open(&path).unwrap();
                    for i in 0..50 {
                        db.with(|c| {
                            insert_message(c, &text(&conv, &owner, &format!("{w}-{i}")))?;
                            crate::entities::create_feed_post(
                                c,
                                &owner,
                                &crate::entities::NewFeedPost {
                                    title: format!("{w}-{i}"),
                                    body: "b".into(),
                                    ..Default::default()
                                },
                            )
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        first
            .with(|c| {
                for table in ["messages", "feed_posts"] {
                    let dupes: i64 = c.query_row(
                        &format!(
                            "SELECT COUNT(*) FROM (SELECT created_at FROM {table}
                             GROUP BY created_at HAVING COUNT(*) > 1)"
                        ),
                        [],
                        |r| r.get(0),
                    )?;
                    assert_eq!(dupes, 0, "{table}");
                }
                // Paging by timestamp walks every post exactly once.
                let mut before = None;
                let mut seen = 0;
                loop {
                    let page = crate::entities::list_feed(c, &owner, before, 7)?;
                    if page.is_empty() {
                        break;
                    }
                    seen += page.len();
                    before = page.last().map(|p| p.created_at);
                }
                assert_eq!(seen, 200);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_database_at_the_release_schema_upgrades_with_connectors_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..3] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute(
                "INSERT INTO connectors (id, name, transport, config, created_at, updated_at)
                 VALUES ('c1', 'old', 'stdio', '{}', 1, 1)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            assert_eq!(v as usize, MIGRATIONS.len());
            let (cwd, policy): (i64, String) = c.query_row(
                "SELECT workspace_cwd, policy FROM connectors WHERE id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!((cwd, policy.as_str()), (1, "{}"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn main_schema_upgrades_with_gateway_data_and_browser_approvals_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        {
            let conn = Connection::open(&path).unwrap();
            // origin/main schema is version 4; branch migrations start after it.
            conn.execute_batch(include_str!("../tests/fixtures/main-schema-v4.sql"))
                .unwrap();
            conn.execute(
                "INSERT INTO connectors (id, name, transport, config, created_at, updated_at, policy)
                 VALUES ('c1', 'saved', 'stdio', '{}', 1, 1, ?1)",
                [r#"{"mode":"ask"}"#],
            ).unwrap();
            conn.execute(
                "INSERT INTO known_recipients VALUES ('reader@example.test', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO engine_keys VALUES ('codex', 'SYNTHETIC_KEY', 'sealed', 1)",
                [],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            assert_eq!(v as usize, MIGRATIONS.len());
            let policy: String =
                c.query_row("SELECT policy FROM connectors WHERE id = 'c1'", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(policy, r#"{"mode":"ask"}"#);
            let key: String =
                c.query_row("SELECT ciphertext FROM engine_keys", [], |r| r.get(0))?;
            assert_eq!(key, "sealed");
            let recipients: i64 =
                c.query_row("SELECT COUNT(*) FROM known_recipients", [], |r| r.get(0))?;
            assert_eq!(recipients, 1);
            c.prepare("SELECT * FROM connector_calls")?;
            let owner = owner_user_id(c)?.unwrap();
            let conv = ensure_main_conversation(c, &owner)?;
            let card = crate::entities::approval_card(&crate::entities::ApprovalRequest {
                service: "wallet",
                approver_user_id: &owner,
                ..Default::default()
            });
            crate::entities::insert_card(c, &conv, &card, "assistant")?;
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM messages INDEXED BY messages_pending_approvals
                WHERE kind = 'card' AND json_extract(card, '$.type') = 'approval'
                  AND json_extract(card, '$.state') = 'pending'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(n, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_database_at_main_schema_upgrades_with_feed_and_gateway_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        let owner;
        {
            let conn = Connection::open(&path).unwrap();
            // Main shipped migrations 1–5; the feed is migration 6.
            conn.execute_batch(include_str!("../tests/fixtures/main-schema-v4.sql"))
                .unwrap();
            conn.execute_batch(include_str!("../tests/fixtures/main-migration-v5.sql"))
                .unwrap();
            owner = ensure_owner(&conn).unwrap();
            let conv = ensure_main_conversation(&conn, &owner).unwrap();
            let card = crate::entities::approval_card(&crate::entities::ApprovalRequest {
                service: "wallet",
                approver_user_id: &owner,
                ..Default::default()
            });
            crate::entities::insert_card(&conn, &conv, &card, "assistant").unwrap();
            conn.execute(
                r#"INSERT INTO connectors (id, name, transport, config, policy, paused_reason,
                     paused_at, auth_state, workspace_cwd, created_at, updated_at)
                 VALUES ('c1', 'mail', 'stdio', '{}', '{"default":"ask"}', 'tripwire',
                     2, 'ok', 0, 1, 1)"#,
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO connector_calls (id, connector_id, connector_name, tool,
                     category, decision, created_at) VALUES ('call1', 'c1', 'mail',
                     'send_message', 'send', 'ask', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                r#"INSERT INTO feed_posts (id, user_id, title, body, images, source_name, why,
                     created_at, updated_at) VALUES ('p1', ?1, 'A story', 'A description',
                     '["https://example.com/photo.jpg"]', 'Example', 'Your interests', 1, 1)"#,
                [&owner],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            assert_eq!(v as usize, MIGRATIONS.len());
            let (policy, reason, cwd): (String, String, i64) = c.query_row(
                "SELECT policy, paused_reason, workspace_cwd FROM connectors WHERE id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(policy, r#"{"default":"ask"}"#);
            assert_eq!(reason, "tripwire");
            assert_eq!(cwd, 0);
            let calls: i64 =
                c.query_row("SELECT COUNT(*) FROM connector_calls", [], |r| r.get(0))?;
            assert_eq!(calls, 1);

            let pending: i64 = c.query_row(
                "SELECT COUNT(*) FROM messages INDEXED BY messages_pending_approvals
                 WHERE kind = 'card' AND json_extract(card, '$.type') = 'approval'
                   AND json_extract(card, '$.state') = 'pending'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(pending, 1);
            let posts = crate::entities::list_feed(c, &owner, None, 10)?;
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0].title, "A story");
            assert_eq!(posts[0].source_name.as_deref(), Some("Example"));
            assert!(posts[0].hook.is_none());
            let source =
                crate::feed::store::add_source(c, &owner, "hn", "front_page", None, None, "user")?;
            assert_eq!(
                crate::feed::store::list_sources(c, &owner)?[0].id,
                source.unwrap().id
            );
            c.execute(
                "INSERT INTO push_queue (id, user_id, payload, created_at)
                VALUES ('push1', ?1, '{}', 1)",
                [&owner],
            )?;
            Ok(())
        })
        .unwrap();
        // Reopening an upgraded DB must not try to apply the feed twice.
        drop(db);
        Db::open(&path).unwrap();
    }

    #[test]
    fn upgrading_spreads_existing_timestamp_ties() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("iris.db");
        {
            // A schema-2 database written before timestamps were unique.
            let mut conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..2] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 2).unwrap();
            let owner = ensure_owner(&conn).unwrap();
            let main = ensure_main_conversation(&conn, &owner).unwrap();
            let side = create_conversation(&conn, &owner, "side", "", None)
                .unwrap()
                .id;
            conn.execute("UPDATE conversation_members SET joined_at = 0", [])
                .unwrap();
            let tx = conn.transaction().unwrap();
            for (i, conv) in [&main, &side, &main, &side].iter().enumerate() {
                tx.execute(
                    "INSERT INTO messages (id, conversation_id, role, kind, content, attachments,
                         status, meta, created_at, updated_at)
                     VALUES (?1, ?2, 'assistant', 'text', ?3, '[]', 'complete',
                         '{\"activity\":[{\"id\":\"a\"}]}', 5000, 5000)",
                    params![format!("m{i}"), conv, format!("reply {i}")],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO feed_posts (id, user_id, title, body, created_at, updated_at)
                     VALUES (?1, ?2, 't', 'b', 7000, 7000)",
                    params![format!("p{i}"), owner],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        // A session that had seen everything up to the ties.
        {
            let conn = Connection::open(&path).unwrap();
            let main: String = conn
                .query_row(
                    "SELECT conversation_id FROM messages WHERE id = 'm0'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO harness_sessions (conversation_id, harness, session_id, synced_at, updated_at)
                 VALUES (?1, 'codex', 's1', 5000, 0)",
                params![main],
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let main: String = c.query_row(
                "SELECT conversation_id FROM messages WHERE id = 'm0'",
                [],
                |r| r.get(0),
            )?;
            let session = get_harness_session(c, &main, "codex")?.unwrap();
            assert!(
                replay_messages(c, &main, session.synced_at, i64::MAX, 40)?.is_empty(),
                "nothing the session saw is replayed"
            );
            // Two per page: every tied row still shows up exactly once.
            let (mut seen, mut before) = (Vec::new(), None);
            loop {
                let page = recent_activity(c, &owner, false, before, 2)?;
                if page.is_empty() {
                    break;
                }
                before = page.last().map(|e| e.created_at);
                seen.extend(page.into_iter().map(|e| e.message_id));
            }
            seen.sort();
            assert_eq!(seen, ["m0", "m1", "m2", "m3"]);
            let (mut posts, mut before) = (0, None);
            loop {
                let page = crate::entities::list_feed(c, &owner, before, 2)?;
                if page.is_empty() {
                    break;
                }
                before = page.last().map(|p| p.created_at);
                posts += page.len();
            }
            assert_eq!(posts, 4);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sent_photos_page_through_photos_of_one_message() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            let owner = owner_user_id(c)?.unwrap();
            let conv = ensure_main_conversation(c, &owner)?;
            let ids: Vec<String> = (0..5).map(|_| new_id()).collect();
            for id in &ids {
                insert_attachment(c, id, &owner, "image/png", 1, "00", &format!("{id}.png"))?;
            }
            for chunk in [&ids[..3], &ids[3..]] {
                insert_message(
                    c,
                    &NewMessage {
                        attachments: chunk,
                        ..text(&conv, &owner, "photos")
                    },
                )?;
            }
            let (mut seen, mut before) = (Vec::new(), None::<(i64, String)>);
            loop {
                let page =
                    sent_photos(c, &owner, before.as_ref().map(|(a, i)| (*a, i.as_str())), 2)?;
                if page.is_empty() {
                    break;
                }
                let last = page.last().unwrap();
                before = Some((last.used_at, last.attachment.id.clone()));
                seen.extend(page.into_iter().map(|p| p.attachment.id));
            }
            let mut want = ids.clone();
            want.sort();
            seen.sort();
            assert_eq!(seen, want);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn settings_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.with(|c| {
            assert_eq!(get_setting(c, "agent_name")?, None);
            set_setting(c, "agent_name", "Nova")?;
            set_setting(c, "agent_name", "Vega")?;
            assert_eq!(get_setting(c, "agent_name")?.as_deref(), Some("Vega"));
            delete_setting(c, "agent_name")?;
            assert_eq!(get_setting(c, "agent_name")?, None);
            Ok(())
        })
        .unwrap();
    }
}
