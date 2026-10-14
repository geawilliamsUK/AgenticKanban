-- Tool calls awaiting the human's decision, raised by the PreToolUse hook.

CREATE TABLE approvals (
    id             TEXT PRIMARY KEY,
    run_id         TEXT NOT NULL,
    agent_actor_id TEXT NOT NULL REFERENCES actors(id),
    tool_name      TEXT NOT NULL,
    input          TEXT NOT NULL,
    summary        TEXT NOT NULL,
    state          TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    decided_by     TEXT,
    decided_at     TEXT
);
CREATE INDEX idx_approvals_state ON approvals(state, created_at);
