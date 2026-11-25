-- One retained catalog identity per game occurrence, including retired cards.
DROP INDEX tactic_game_source;
CREATE UNIQUE INDEX tactic_game_source ON tactics(game_id,game_ply);

CREATE TABLE capture_qualification (
  game_id TEXT NOT NULL,
  ply INTEGER NOT NULL,
  detector_version INTEGER NOT NULL,
  policy_version INTEGER NOT NULL DEFAULT 1 CHECK(policy_version=1),
  outcome TEXT CHECK(outcome IN ('kept','narrowed','published','equivalent','best_outside_candidates','played_best')),
  evidence TEXT,
  retry_at INTEGER NOT NULL DEFAULT 0,
  failures INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  PRIMARY KEY(game_id,ply),
  FOREIGN KEY(game_id,ply,detector_version) REFERENCES missed_captures(game_id,ply,detector_version)
) STRICT;
CREATE INDEX capture_qualification_ready ON capture_qualification(retry_at,game_id,ply)
  WHERE outcome IS NULL;

INSERT INTO capture_qualification(game_id,ply,detector_version)
  SELECT game_id,ply,min(detector_version) FROM missed_captures GROUP BY game_id,ply;

CREATE TRIGGER capture_qualification_candidate AFTER INSERT ON missed_captures BEGIN
  INSERT INTO capture_qualification(game_id,ply,detector_version)
    VALUES(new.game_id,new.ply,new.detector_version)
    ON CONFLICT(game_id,ply) DO NOTHING;
END;

-- All retained findings now enter qualification. Preserve historical job records.
UPDATE jobs SET status='done',error=NULL WHERE provider='archive' AND operation='publish' AND status!='done';
UPDATE job_games SET scanned=1 WHERE scanned=0 AND job_id IN (
  SELECT id FROM jobs WHERE provider='archive' AND operation='publish'
);
