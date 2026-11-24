ALTER TABLE cards ADD COLUMN quarantined INTEGER NOT NULL DEFAULT 0
  CHECK (quarantined IN (0, 1) AND (kind = 'opening' OR quarantined = 0));

UPDATE cards SET quarantined = 1
WHERE kind = 'opening' AND (repertoire_id, position_id) IN (
  SELECT repertoire_id, position_id FROM cards
  WHERE kind = 'opening'
  GROUP BY repertoire_id, position_id HAVING COUNT(*) > 1
);

CREATE UNIQUE INDEX active_opening_response ON cards(repertoire_id, position_id)
  WHERE kind = 'opening' AND quarantined = 0;

CREATE VIEW active_cards AS SELECT * FROM cards WHERE quarantined = 0;

DELETE FROM deck WHERE card_id IN (SELECT id FROM cards WHERE quarantined = 1);
CREATE TABLE new_opening_settings (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  paused INTEGER NOT NULL CHECK (paused IN (0, 1)),
  ordering TEXT NOT NULL CHECK (ordering IN ('maia', 'standard'))
) STRICT;

INSERT INTO new_opening_settings VALUES (1, 0, 'maia');
PRAGMA user_version = 1;
