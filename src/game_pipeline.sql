ALTER TABLE jobs ADD COLUMN parent_id INTEGER REFERENCES jobs(id);
CREATE UNIQUE INDEX job_child_operation ON jobs(parent_id, operation);
CREATE INDEX job_pending_games ON job_games(job_id, scanned, game_id);
ALTER TABLE missed_captures ADD COLUMN fen TEXT;
CREATE INDEX tactic_game_source ON tactics(game_id, game_ply);
PRAGMA user_version = 2;
