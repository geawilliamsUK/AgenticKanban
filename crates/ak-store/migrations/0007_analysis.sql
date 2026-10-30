-- Per-run outcomes and the scheduler's skip decisions, for the team analyst.

ALTER TABLE runs ADD COLUMN outcome TEXT;

CREATE TABLE scheduler_decisions (
    seq            INTEGER PRIMARY KEY AUTOINCREMENT,
    at             TEXT NOT NULL,
    agent_actor_id TEXT NOT NULL REFERENCES actors(id),
    reason         TEXT NOT NULL,
    detail         TEXT NOT NULL
);
CREATE INDEX idx_scheduler_decisions_at ON scheduler_decisions(at);
