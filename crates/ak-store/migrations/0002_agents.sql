-- Agents: a cache of each definition file plus runtime state.

CREATE TABLE agents (
    actor_id           TEXT PRIMARY KEY REFERENCES actors(id),
    description        TEXT NOT NULL DEFAULT '',
    model              TEXT,
    permission_profile TEXT NOT NULL,
    budgets            TEXT,
    state              TEXT NOT NULL DEFAULT 'idle',
    current_run_id     TEXT,
    cooldown_until     TEXT,
    missing_since      TEXT,
    definition_path    TEXT NOT NULL,
    memory_path        TEXT NOT NULL
);
