-- Agent runs and their parsed transcripts.

CREATE TABLE runs (
    id                TEXT PRIMARY KEY,
    agent_actor_id    TEXT NOT NULL REFERENCES actors(id),
    trigger           TEXT NOT NULL,
    claude_session_id TEXT,
    workspace_path    TEXT NOT NULL,
    started_at        TEXT NOT NULL,
    ended_at          TEXT,
    exit_reason       TEXT,
    num_turns         INTEGER NOT NULL DEFAULT 0,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL,
    error             TEXT
);
CREATE INDEX idx_runs_agent ON runs(agent_actor_id, started_at);
CREATE INDEX idx_runs_started ON runs(started_at);

CREATE TABLE run_events (
    run_id  TEXT NOT NULL REFERENCES runs(id),
    seq     INTEGER NOT NULL,
    at      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);
