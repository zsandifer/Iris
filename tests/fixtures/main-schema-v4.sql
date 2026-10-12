-- Schema from origin/main before browser migrations. Synthetic data only.

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


-- Feed posts carry the source page's original photos (first one is also
-- image_url), the source's display name, and a "why you'd like this" line.
ALTER TABLE feed_posts ADD COLUMN images TEXT NOT NULL DEFAULT '[]';
ALTER TABLE feed_posts ADD COLUMN source_name TEXT;
ALTER TABLE feed_posts ADD COLUMN why TEXT;
UPDATE feed_posts SET images = json_array(image_url) WHERE image_url IS NOT NULL;
CREATE INDEX feed_posts_liked ON feed_posts(user_id, liked);

-- Goals the agent suggested and the user has not confirmed yet.
ALTER TABLE goals ADD COLUMN suggested INTEGER NOT NULL DEFAULT 0;


-- Message timestamps are unique across conversations (they are the
-- activity log's page cursor); this keeps finding the newest one cheap.
CREATE INDEX messages_created ON messages(created_at);


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

PRAGMA user_version = 4;
