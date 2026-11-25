ALTER TABLE cards ADD COLUMN retired INTEGER NOT NULL DEFAULT 0
  CHECK (retired IN (0,1) AND (retired=0 OR (kind='tactic' AND source_game_id IS NOT NULL AND source_ply IS NOT NULL)));
ALTER TABLE attempts ADD COLUMN issued_answers TEXT;

DROP VIEW active_cards;
CREATE VIEW active_cards AS SELECT * FROM cards WHERE quarantined=0 AND retired=0;

DROP TRIGGER prepare_quarantine;
CREATE TRIGGER prepare_card_availability AFTER UPDATE OF quarantined,retired ON cards BEGIN
  UPDATE prepared_queue SET active=NOT (new.quarantined OR new.retired) WHERE card_id=new.id;
END;

