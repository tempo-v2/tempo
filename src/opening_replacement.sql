ALTER TABLE openings ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE cards ADD COLUMN opening_retired INTEGER NOT NULL DEFAULT 0
  CHECK(opening_retired IN (0,1) AND (opening_retired=0 OR kind='opening'));

CREATE INDEX active_opening_lines ON openings(repertoire_id,color,id) WHERE archived=0;
DROP INDEX active_opening_response;
CREATE UNIQUE INDEX active_opening_response ON cards(repertoire_id,position_id)
  WHERE kind='opening' AND quarantined=0 AND opening_retired=0;
CREATE INDEX current_opening_reply ON cards(repertoire_id,position_id,quarantined)
  WHERE kind='opening' AND opening_retired=0;

DROP VIEW active_cards;
CREATE VIEW active_cards AS
  SELECT * FROM cards WHERE quarantined=0 AND retired=0 AND opening_retired=0;
DROP TRIGGER prepare_card_availability;
CREATE TRIGGER prepare_card_availability AFTER UPDATE OF quarantined,retired,opening_retired ON cards BEGIN
  UPDATE prepared_queue SET active=NOT (new.quarantined OR new.retired OR new.opening_retired)
    WHERE card_id=new.id;
END;
CREATE TRIGGER opening_reply_retirement AFTER UPDATE OF opening_retired ON cards
WHEN new.opening_retired<>old.opening_retired BEGIN
  INSERT INTO coverage_revision VALUES(new.repertoire_id,1)
    ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;

-- Adding a line with existing replies affects replacement confirmation only.
CREATE TABLE opening_line_revision(repertoire TEXT PRIMARY KEY, revision INTEGER NOT NULL) STRICT;
CREATE TRIGGER opening_line_insert AFTER INSERT ON openings BEGIN
  INSERT INTO opening_line_revision VALUES(new.repertoire_id,1)
    ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER opening_line_archive AFTER UPDATE OF archived ON openings
WHEN new.archived<>old.archived BEGIN
  INSERT INTO opening_line_revision VALUES(new.repertoire_id,1)
    ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;
