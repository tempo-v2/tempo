ALTER TABLE jobs ADD COLUMN detector_game TEXT REFERENCES games(id);
ALTER TABLE jobs ADD COLUMN detector_version INTEGER NOT NULL DEFAULT 1 CHECK(detector_version>=1);

CREATE TABLE mismatches_v7 (
    game_id TEXT NOT NULL REFERENCES games(id), ply INTEGER NOT NULL,
    position_id TEXT NOT NULL, repertoire_id TEXT NOT NULL, answer_version TEXT NOT NULL,
    played_move TEXT NOT NULL, detector_version INTEGER NOT NULL,
    PRIMARY KEY(game_id,ply,repertoire_id,answer_version,detector_version)
) STRICT;
INSERT INTO mismatches_v7 SELECT *,1 FROM mismatches;
DROP TABLE mismatches;
ALTER TABLE mismatches_v7 RENAME TO mismatches;
CREATE INDEX mismatch_card ON mismatches(position_id,repertoire_id,answer_version);

CREATE TABLE missed_captures_v7 (
    game_id TEXT NOT NULL REFERENCES games(id), ply INTEGER NOT NULL,
    position_id TEXT NOT NULL, played_move TEXT NOT NULL, captures TEXT NOT NULL,
    fen TEXT, detector_version INTEGER NOT NULL,
    PRIMARY KEY(game_id,ply,detector_version)
) STRICT;
INSERT INTO missed_captures_v7 SELECT *,1 FROM missed_captures;
DROP TABLE missed_captures;
ALTER TABLE missed_captures_v7 RENAME TO missed_captures;

-- An unfinished imported-game scan keeps its job and publication child.
-- Its comparison sibling inherits only the addresses yet to finish.
INSERT INTO jobs(provider,username,operation,status,parent_id)
SELECT 'archive','','compare_new','queued',parent_id FROM jobs
WHERE provider='archive' AND operation='scan_new' AND status!='done';

INSERT INTO job_games(job_id,game_id,scanned)
SELECT comparison.id,pending.game_id,0
FROM jobs original
JOIN jobs comparison ON comparison.parent_id=original.parent_id AND comparison.operation='compare_new'
JOIN job_games pending ON pending.job_id=original.id AND pending.scanned=0
WHERE original.provider='archive' AND original.operation='scan_new' AND original.status!='done';

UPDATE jobs SET operation='capture_new'
WHERE provider='archive' AND operation='scan_new' AND status!='done';

CREATE INDEX jobs_visible_latest ON jobs(id)
WHERE operation NOT IN ('compare_all','capture_all');
CREATE INDEX jobs_visible_failed ON jobs(id)
WHERE status='failed' AND operation NOT IN ('compare_all','capture_all');
