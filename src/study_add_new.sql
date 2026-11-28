ALTER TABLE daily_day ADD COLUMN opening_extra INTEGER NOT NULL DEFAULT 0 CHECK(opening_extra>=0);
ALTER TABLE daily_day ADD COLUMN tactic_extra INTEGER NOT NULL DEFAULT 0 CHECK(tactic_extra>=0);
ALTER TABLE daily_day ADD COLUMN opening_new_head INTEGER NOT NULL DEFAULT 0;
ALTER TABLE daily_day ADD COLUMN tactic_new_head INTEGER NOT NULL DEFAULT 0;
ALTER TABLE daily_day ADD COLUMN opening_charged INTEGER NOT NULL DEFAULT 0;
ALTER TABLE daily_day ADD COLUMN tactic_charged INTEGER NOT NULL DEFAULT 0;
ALTER TABLE daily_day ADD COLUMN tactic_first_grades INTEGER NOT NULL DEFAULT 0;
ALTER TABLE daily_clock ADD COLUMN new_generation INTEGER NOT NULL DEFAULT 0;

DROP TRIGGER daily_new_learned;
DROP INDEX daily_new_head;
ALTER TABLE daily_new RENAME TO daily_new_previous;
CREATE TABLE daily_new (
  day TEXT NOT NULL REFERENCES daily_day(day),
  card_id TEXT NOT NULL REFERENCES cards(id),
  kind TEXT NOT NULL CHECK(kind IN ('opening','tactic')),
  ordinal INTEGER NOT NULL,
  availability_version INTEGER NOT NULL,
  parents TEXT NOT NULL CHECK(json_valid(parents)),
  waiting INTEGER NOT NULL CHECK(waiting>=0),
  committed INTEGER NOT NULL CHECK(committed IN (0,1)),
  generation INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(day,generation,card_id)
) STRICT;
INSERT INTO daily_new SELECT *,0 FROM daily_new_previous;
DROP TABLE daily_new_previous;
CREATE INDEX daily_new_head ON daily_new(day,kind,generation,ordinal,card_id)
  WHERE committed=0 AND waiting=0;
CREATE INDEX daily_new_lane ON daily_new(day,kind,generation,committed);
UPDATE daily_day SET
  opening_charged=(SELECT count(*) FROM daily_new n WHERE n.day=daily_day.day AND n.kind='opening' AND n.committed=1),
  tactic_charged=(SELECT count(*) FROM daily_new n WHERE n.day=daily_day.day AND n.kind='tactic' AND n.committed=1);
CREATE TABLE daily_new_size (
  day TEXT NOT NULL REFERENCES daily_day(day),
  kind TEXT NOT NULL,
  generation INTEGER NOT NULL,
  total INTEGER NOT NULL CHECK(total>=0),
  ready INTEGER NOT NULL CHECK(ready BETWEEN 0 AND total),
  PRIMARY KEY(day,kind,generation)
) STRICT, WITHOUT ROWID;
INSERT INTO daily_new_size
SELECT day,kind,generation,count(*),sum(waiting=0) FROM daily_new WHERE committed=0
GROUP BY day,kind,generation;
CREATE INDEX daily_new_empty ON daily_new_size(day) WHERE total=0;
CREATE TRIGGER daily_new_insert AFTER INSERT ON daily_new BEGIN
  INSERT INTO daily_new_size SELECT new.day,new.kind,new.generation,1,(new.waiting=0) WHERE new.committed=0
    ON CONFLICT(day,kind,generation) DO UPDATE SET total=total+1,ready=ready+(new.waiting=0);
  UPDATE daily_day SET opening_charged=opening_charged+(new.kind='opening'),
    tactic_charged=tactic_charged+(new.kind='tactic') WHERE day=new.day AND new.committed=1;
END;
CREATE TRIGGER daily_new_delete AFTER DELETE ON daily_new WHEN old.committed=0 BEGIN
  UPDATE daily_new_size SET total=total-1,ready=ready-(old.waiting=0)
    WHERE day=old.day AND kind=old.kind AND generation=old.generation;
END;
CREATE TRIGGER daily_new_waiting AFTER UPDATE OF waiting ON daily_new WHEN old.committed=0 BEGIN
  UPDATE daily_new_size SET ready=ready+(new.waiting=0)-(old.waiting=0)
    WHERE day=old.day AND kind=old.kind AND generation=old.generation;
END;
CREATE TRIGGER daily_new_learned AFTER INSERT ON learning BEGIN
  DELETE FROM daily_new WHERE day=(SELECT current_day FROM daily_clock)
    AND card_id=new.card_id AND committed=0
    AND generation IN (
      SELECT CASE (SELECT kind FROM cards WHERE id=new.card_id)
        WHEN 'opening' THEN opening_new_head ELSE tactic_new_head END
      FROM daily_day WHERE day=(SELECT current_day FROM daily_clock)
    );
END;

CREATE TABLE study_add_new (
  request TEXT PRIMARY KEY,
  day TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('opening','tactic')),
  quantity INTEGER NOT NULL CHECK(quantity>0),
  added INTEGER CHECK(added BETWEEN 0 AND quantity),
  expires INTEGER NOT NULL,
  outcome TEXT CHECK(outcome IN ('added','expired','paused','unavailable')),
  CHECK((outcome IS 'added')=(added IS NOT NULL))
) STRICT;
CREATE INDEX study_add_new_pending ON study_add_new(day,expires) WHERE outcome IS NULL;
CREATE INDEX study_add_new_day ON study_add_new(day);

ALTER TABLE opening_priority ADD COLUMN completion_cards BLOB CHECK(length(completion_cards)<=524288);
ALTER TABLE opening_priority_projection ADD COLUMN allowance INTEGER NOT NULL DEFAULT 20;
UPDATE opening_priority_projection SET allowance=coalesce(
  (SELECT opening_limit FROM daily_day WHERE day=opening_priority_projection.day),20);
CREATE TRIGGER daily_new_commit AFTER UPDATE OF committed ON daily_new
WHEN old.committed=0 AND new.committed=1 BEGIN
  UPDATE daily_new_size SET total=total-1,ready=ready-(old.waiting=0)
    WHERE day=old.day AND kind=old.kind AND generation=old.generation;
  UPDATE daily_day SET opening_charged=opening_charged+(new.kind='opening'),
    tactic_charged=tactic_charged+(new.kind='tactic') WHERE day=new.day;
END;
CREATE TRIGGER daily_new_tactic_grade AFTER INSERT ON learning
WHEN (SELECT kind FROM cards WHERE id=new.card_id)='tactic' BEGIN
  UPDATE daily_day SET tactic_first_grades=tactic_first_grades+1 WHERE day=(SELECT current_day FROM daily_clock);
END;
CREATE TABLE daily_new_stage (
  generation INTEGER PRIMARY KEY,
  day TEXT NOT NULL REFERENCES daily_day(day),
  kind TEXT NOT NULL,
  expires INTEGER
) STRICT;
CREATE INDEX daily_new_stage_day ON daily_new_stage(day);
CREATE INDEX daily_new_stage_expiry ON daily_new_stage(expires);

ALTER TABLE opening_priority_projection ADD COLUMN prefix_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE opening_priority_projection ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
CREATE TABLE daily_new_prefix (
  generation INTEGER NOT NULL REFERENCES daily_new_stage(generation),
  ordinal INTEGER NOT NULL,
  payload BLOB NOT NULL CHECK(length(payload)<=32768),
  PRIMARY KEY(generation,ordinal)
) STRICT, WITHOUT ROWID;

-- Staged opening slots carry an indexed reverse dependency for each pending
-- parent. The active generation is the only one the Study worker can unlock.
CREATE TABLE daily_new_parent (
  generation INTEGER NOT NULL REFERENCES daily_new_stage(generation),
  parent TEXT NOT NULL,
  child TEXT NOT NULL,
  PRIMARY KEY(generation,parent,child)
) STRICT, WITHOUT ROWID;
CREATE TABLE daily_new_unlock (
  generation INTEGER NOT NULL REFERENCES daily_new_stage(generation),
  parent TEXT NOT NULL,
  PRIMARY KEY(generation,parent)
) STRICT, WITHOUT ROWID;
