-- Refine and Integrate calls: proposal, cost and decision.

CREATE TABLE utility_calls (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    agent_actor_id  TEXT NOT NULL REFERENCES actors(id),
    target_actor_id TEXT REFERENCES actors(id),
    model           TEXT,
    started_at      TEXT NOT NULL,
    ended_at        TEXT,
    cost_usd        REAL,
    state           TEXT NOT NULL,
    proposal        TEXT,
    warnings        TEXT NOT NULL DEFAULT '[]',
    error           TEXT
);
CREATE INDEX idx_utility_calls_agent ON utility_calls(agent_actor_id, started_at);
CREATE INDEX idx_utility_calls_started ON utility_calls(started_at);
