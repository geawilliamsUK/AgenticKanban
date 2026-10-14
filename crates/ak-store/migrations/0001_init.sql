-- One database per project. Ids are UUID strings, timestamps are RFC 3339 text,
-- structured payloads are JSON text. Nothing here is SQLite-specific beyond
-- AUTOINCREMENT on the event sequence.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE actors (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    handle       TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL
);

CREATE TABLE columns (
    id       TEXT PRIMARY KEY,
    name     TEXT NOT NULL,
    position INTEGER NOT NULL,
    kind     TEXT NOT NULL
);

CREATE TABLE tickets (
    id            TEXT PRIMARY KEY,
    key           TEXT NOT NULL UNIQUE,
    key_number    INTEGER NOT NULL,
    title         TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    column_id     TEXT NOT NULL REFERENCES columns(id),
    position      INTEGER NOT NULL,
    assignee_id   TEXT REFERENCES actors(id),
    reporter_id   TEXT NOT NULL REFERENCES actors(id),
    priority      TEXT NOT NULL,
    labels        TEXT NOT NULL DEFAULT '[]',
    parent_id     TEXT REFERENCES tickets(id),
    frozen        INTEGER NOT NULL DEFAULT 0,
    freeze_reason TEXT,
    version       INTEGER NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX idx_tickets_column ON tickets(column_id, position);
CREATE INDEX idx_tickets_assignee ON tickets(assignee_id);

CREATE TABLE comments (
    id                TEXT PRIMARY KEY,
    ticket_id         TEXT NOT NULL REFERENCES tickets(id),
    author_id         TEXT NOT NULL REFERENCES actors(id),
    body              TEXT NOT NULL,
    parent_comment_id TEXT REFERENCES comments(id),
    created_at        TEXT NOT NULL
);
CREATE INDEX idx_comments_ticket ON comments(ticket_id, created_at);

CREATE TABLE watchers (
    ticket_id TEXT NOT NULL REFERENCES tickets(id),
    actor_id  TEXT NOT NULL REFERENCES actors(id),
    PRIMARY KEY (ticket_id, actor_id)
);

CREATE TABLE events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    id           TEXT NOT NULL UNIQUE,
    occurred_at  TEXT NOT NULL,
    actor_id     TEXT NOT NULL,
    kind         TEXT NOT NULL,
    subject_kind TEXT NOT NULL,
    subject_id   TEXT NOT NULL,
    payload      TEXT NOT NULL
);
CREATE INDEX idx_events_subject ON events(subject_kind, subject_id, seq);

CREATE TABLE notifications (
    id                TEXT PRIMARY KEY,
    recipient_id      TEXT NOT NULL REFERENCES actors(id),
    event_id          TEXT NOT NULL REFERENCES events(id),
    kind              TEXT NOT NULL,
    wake              INTEGER NOT NULL,
    state             TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    handled_at        TEXT,
    handled_in_run_id TEXT
);
CREATE INDEX idx_notifications_recipient ON notifications(recipient_id, state, created_at);
