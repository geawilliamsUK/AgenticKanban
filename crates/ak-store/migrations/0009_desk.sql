-- The Desk: the human's own Claude Code conversations about the project.

CREATE TABLE desk_sessions (
    id                TEXT PRIMARY KEY,
    title             TEXT NOT NULL,
    claude_session_id TEXT NOT NULL,
    started           INTEGER NOT NULL DEFAULT 0,
    model             TEXT,
    created_at        TEXT NOT NULL,
    last_active_at    TEXT NOT NULL,
    cost_usd          REAL NOT NULL DEFAULT 0,
    archived          INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE desk_entries (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES desk_sessions(id),
    at         TEXT NOT NULL,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX idx_desk_entries_session ON desk_entries(session_id, seq);
