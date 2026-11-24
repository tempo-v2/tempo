ALTER TABLE jobs ADD COLUMN window_offset INTEGER NOT NULL DEFAULT 0 CHECK(window_offset >= 0);
ALTER TABLE jobs ADD COLUMN window_rejected INTEGER NOT NULL DEFAULT 0 CHECK(window_rejected >= 0);

CREATE TABLE repertoire_choices (
    repertoire_id TEXT NOT NULL,
    color TEXT NOT NULL,
    example_line TEXT NOT NULL,
    PRIMARY KEY(repertoire_id,color)
) WITHOUT ROWID, STRICT;

INSERT INTO repertoire_choices
SELECT repertoire_id,color,name FROM openings
WHERE rowid IN (SELECT min(rowid) FROM openings GROUP BY repertoire_id,color);

CREATE TRIGGER opening_repertoire_choice AFTER INSERT ON openings BEGIN
    INSERT OR IGNORE INTO repertoire_choices VALUES (new.repertoire_id,new.color,new.name);
END;

PRAGMA user_version=5;
