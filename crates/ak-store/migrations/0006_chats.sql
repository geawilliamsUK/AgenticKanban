-- Chats are containers; every message lives in a thread.

CREATE TABLE chats (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    name       TEXT,
    created_by TEXT NOT NULL REFERENCES actors(id),
    created_at TEXT NOT NULL
);

CREATE TABLE chat_members (
    chat_id  TEXT NOT NULL REFERENCES chats(id),
    actor_id TEXT NOT NULL REFERENCES actors(id),
    PRIMARY KEY (chat_id, actor_id)
);
CREATE INDEX idx_chat_members_actor ON chat_members(actor_id);

CREATE TABLE threads (
    id               TEXT PRIMARY KEY,
    chat_id          TEXT NOT NULL REFERENCES chats(id),
    title            TEXT NOT NULL,
    created_by       TEXT NOT NULL REFERENCES actors(id),
    created_at       TEXT NOT NULL,
    last_activity_at TEXT NOT NULL,
    paused           INTEGER NOT NULL DEFAULT 0,
    archived         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_threads_chat ON threads(chat_id, last_activity_at);

CREATE TABLE thread_participants (
    thread_id TEXT NOT NULL REFERENCES threads(id),
    actor_id  TEXT NOT NULL REFERENCES actors(id),
    PRIMARY KEY (thread_id, actor_id)
);

CREATE TABLE messages (
    id         TEXT PRIMARY KEY,
    thread_id  TEXT NOT NULL REFERENCES threads(id),
    author_id  TEXT NOT NULL REFERENCES actors(id),
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX idx_messages_thread ON messages(thread_id, created_at);

-- Full-text search. Standalone tables, written in the same transaction as
-- the rows they index, so nothing depends on rowids staying put.
CREATE VIRTUAL TABLE threads_fts USING fts5(title, thread_id UNINDEXED);
CREATE VIRTUAL TABLE messages_fts USING fts5(body, thread_id UNINDEXED, message_id UNINDEXED);
