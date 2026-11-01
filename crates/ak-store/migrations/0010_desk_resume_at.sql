-- Where an edited Desk session branches its Claude Code session from.

ALTER TABLE desk_sessions ADD COLUMN resume_at TEXT;
