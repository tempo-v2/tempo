CREATE INDEX repertoire_choices_color ON repertoire_choices(color,repertoire_id);
CREATE INDEX legacy_game_mismatches ON mismatches(game_id,repertoire_id)
    WHERE detector_version=1;

-- A feeder keeps its game and repertoire position across worker turns.
ALTER TABLE jobs ADD COLUMN feedback_cursor TEXT;
CREATE TABLE repertoire_feedback (
    game_id TEXT NOT NULL REFERENCES games(id),
    repertoire_id TEXT NOT NULL,
    job_id INTEGER NOT NULL UNIQUE REFERENCES jobs(id),
    generation INTEGER NOT NULL CHECK(generation>0),
    completed_generation INTEGER CHECK(completed_generation BETWEEN 1 AND generation),
    repertoire_revision INTEGER,
    comparator_version INTEGER,
    report BLOB CHECK(length(report)<=262144),
    CHECK((report IS NULL) = (completed_generation IS NULL)),
    CHECK((report IS NULL) = (repertoire_revision IS NULL)),
    CHECK((report IS NULL) = (comparator_version IS NULL)),
    PRIMARY KEY(game_id,repertoire_id)
) STRICT;

CREATE INDEX jobs_active_feedback ON jobs(id)
    WHERE operation='compare_game' AND status IN ('queued','running');
DROP INDEX jobs_active_user;
CREATE INDEX jobs_active_user ON jobs(status,provider,operation,username,id)
    WHERE parent_id IS NULL AND operation NOT IN ('publish','compare_game')
      AND status IN ('queued','running');
DROP INDEX jobs_visible_latest;
CREATE INDEX jobs_visible_latest ON jobs(id)
    WHERE operation NOT IN ('compare_all','capture_all','compare_game');
DROP INDEX jobs_visible_failed;
CREATE INDEX jobs_visible_failed ON jobs(id)
    WHERE status='failed' AND operation NOT IN ('compare_all','capture_all','compare_game');
