ALTER TABLE daily_day ADD COLUMN mix_balance INTEGER NOT NULL DEFAULT 0;

-- Counts track the same prepared rows that the published head can address.
-- An empty generation needs no count row.
CREATE TABLE daily_due_size (
  day TEXT NOT NULL REFERENCES daily_day(day),
  generation INTEGER NOT NULL,
  remaining INTEGER NOT NULL CHECK(remaining>0),
  PRIMARY KEY(day,generation)
) STRICT, WITHOUT ROWID;

INSERT INTO daily_due_size
SELECT day,generation,count(*) FROM daily_due GROUP BY day,generation;

CREATE TRIGGER daily_due_added AFTER INSERT ON daily_due BEGIN
  INSERT INTO daily_due_size VALUES(new.day,new.generation,1)
    ON CONFLICT(day,generation) DO UPDATE SET remaining=remaining+1;
END;

CREATE TRIGGER daily_due_removed AFTER DELETE ON daily_due BEGIN
  DELETE FROM daily_due_size WHERE day=old.day AND generation=old.generation AND remaining=1;
  UPDATE daily_due_size SET remaining=remaining-1 WHERE day=old.day AND generation=old.generation;
END;

-- A first grade outside the daily plan can remove fresh eligibility. A slot
-- already charged at issue or reservation must stay charged.
DELETE FROM daily_new WHERE day=(SELECT current_day FROM daily_clock) AND committed=0
  AND EXISTS(SELECT 1 FROM learning WHERE card_id=daily_new.card_id);

CREATE TRIGGER daily_new_learned AFTER INSERT ON learning BEGIN
  DELETE FROM daily_new WHERE day=(SELECT current_day FROM daily_clock)
    AND card_id=new.card_id AND committed=0;
END;
