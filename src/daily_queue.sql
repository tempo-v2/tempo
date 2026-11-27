ALTER TABLE new_opening_settings ADD COLUMN opening_limit INTEGER NOT NULL DEFAULT 20 CHECK(opening_limit BETWEEN 0 AND 20);
ALTER TABLE new_opening_settings ADD COLUMN tactic_limit INTEGER NOT NULL DEFAULT 20 CHECK(tactic_limit BETWEEN 0 AND 20);
ALTER TABLE prepared_queue ADD COLUMN availability_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE prepared_queue ADD COLUMN schedule_version INTEGER NOT NULL DEFAULT 0;

CREATE TABLE daily_clock (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  sequence INTEGER NOT NULL DEFAULT 0,
  openings INTEGER NOT NULL DEFAULT 0,
  authoring INTEGER NOT NULL DEFAULT 0,
  tactics INTEGER NOT NULL DEFAULT 0,
  coverage INTEGER NOT NULL DEFAULT 0,
  due_generation INTEGER NOT NULL DEFAULT 0,
  current_day TEXT,
  last_order_day TEXT,
  cutover_day TEXT
) STRICT;
INSERT INTO daily_clock(singleton) VALUES(1);

CREATE TABLE daily_day (
  day TEXT PRIMARY KEY,
  cutoff INTEGER NOT NULL,
  high_water INTEGER NOT NULL,
  opening_limit INTEGER NOT NULL CHECK(opening_limit BETWEEN 0 AND 20),
  tactic_limit INTEGER NOT NULL CHECK(tactic_limit BETWEEN 0 AND 20),
  mode TEXT NOT NULL CHECK(mode IN ('maia','standard')),
  due_head INTEGER,
  due_coverage INTEGER,
  openings_version INTEGER,
  tactics_version INTEGER,
  opening_order TEXT NOT NULL DEFAULT 'standard' CHECK(opening_order IN ('maia','saved','standard')),
  fresh_order INTEGER NOT NULL DEFAULT 0 CHECK(fresh_order IN (0,1)),
  order_profile TEXT,
  order_staged INTEGER NOT NULL DEFAULT 0,
  forecast_finished INTEGER NOT NULL DEFAULT 0 CHECK(forecast_finished IN (0,1)),
  commitments INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE daily_due (
  day TEXT NOT NULL REFERENCES daily_day(day),
  generation INTEGER NOT NULL,
  card_id TEXT NOT NULL REFERENCES cards(id),
  kind TEXT NOT NULL CHECK(kind IN ('opening','tactic')),
  ordinal INTEGER NOT NULL,
  schedule_version INTEGER NOT NULL,
  PRIMARY KEY(day,generation,card_id),
  UNIQUE(day,generation,kind,ordinal)
) STRICT;
CREATE INDEX daily_due_card ON daily_due(card_id,day);
CREATE TABLE daily_new (
  day TEXT NOT NULL REFERENCES daily_day(day),
  card_id TEXT NOT NULL REFERENCES cards(id),
  kind TEXT NOT NULL CHECK(kind IN ('opening','tactic')),
  ordinal INTEGER NOT NULL,
  availability_version INTEGER NOT NULL,
  parents TEXT NOT NULL CHECK(json_valid(parents)),
  waiting INTEGER NOT NULL CHECK(waiting BETWEEN 0 AND 20),
  committed INTEGER NOT NULL CHECK(committed IN (0,1)),
  PRIMARY KEY(day,card_id)
) STRICT;
CREATE INDEX daily_new_head ON daily_new(day,kind,ordinal,card_id) WHERE committed=0 AND waiting=0;
CREATE TABLE daily_opening_order (
  day TEXT NOT NULL REFERENCES daily_day(day),
  card_id TEXT NOT NULL REFERENCES cards(id),
  ordinal INTEGER NOT NULL,
  PRIMARY KEY(day,card_id),
  UNIQUE(day,ordinal)
) STRICT;

-- Versions are issued by the Store projection. Original data rows retain their shape.
CREATE TRIGGER daily_available_insert AFTER INSERT ON prepared_queue BEGIN
  UPDATE daily_clock SET sequence=sequence+1, openings=openings+(new.active=1 AND new.kind='opening'),
    authoring=authoring+(new.active=1 AND new.kind='opening'),
    tactics=tactics+(new.active=1 AND new.kind='tactic');
  UPDATE prepared_queue SET availability_version=(SELECT sequence FROM daily_clock)
    WHERE card_id=new.card_id;
END;
CREATE TRIGGER daily_available_change AFTER UPDATE OF active ON prepared_queue
WHEN new.active<>old.active BEGIN
  UPDATE daily_clock SET sequence=sequence+1, openings=openings+(new.kind='opening'),
    authoring=authoring+(new.kind='opening'),
    tactics=tactics+(new.kind='tactic');
  UPDATE prepared_queue SET availability_version=(SELECT sequence FROM daily_clock)
    WHERE card_id=new.card_id;
  DELETE FROM daily_due WHERE card_id=new.card_id AND day=(SELECT current_day FROM daily_clock)
    AND generation IN ((SELECT due_generation FROM daily_clock),
      (SELECT due_head FROM daily_day WHERE day=(SELECT current_day FROM daily_clock)));
END;
CREATE TRIGGER daily_definition_change AFTER UPDATE OF answers ON cards
WHEN new.answers<>old.answers BEGIN
  UPDATE daily_clock SET sequence=sequence+1, openings=openings+(new.kind='opening'),
    authoring=authoring+(new.kind='opening'),
    tactics=tactics+(new.kind='tactic');
  UPDATE prepared_queue SET availability_version=(SELECT sequence FROM daily_clock)
    WHERE card_id=new.id;
  DELETE FROM daily_due WHERE card_id=new.id AND day=(SELECT current_day FROM daily_clock)
    AND generation IN ((SELECT due_generation FROM daily_clock),
      (SELECT due_head FROM daily_day WHERE day=(SELECT current_day FROM daily_clock)));
END;
CREATE TRIGGER daily_routes_insert AFTER INSERT ON opening_line_revision BEGIN
  UPDATE daily_clock SET openings=openings+1,authoring=authoring+1;
END;
CREATE TRIGGER daily_routes_change AFTER UPDATE OF revision ON opening_line_revision BEGIN
  UPDATE daily_clock SET openings=openings+1,authoring=authoring+1;
END;
CREATE TRIGGER daily_coverage_order AFTER UPDATE OF bank ON forecast_order_state
WHEN new.bank<>old.bank BEGIN
  UPDATE daily_clock SET coverage=coverage+1;
END;

DROP TRIGGER prepare_due_insert;
DROP TRIGGER prepare_due_update;
DROP TRIGGER prepare_due_delete;
CREATE TRIGGER prepare_due_insert AFTER INSERT ON learning BEGIN
  UPDATE daily_clock SET sequence=sequence+1;
  UPDATE prepared_queue SET due_at=new.due_at,schedule_version=(SELECT sequence FROM daily_clock) WHERE card_id=new.card_id;
END;
CREATE TRIGGER prepare_due_update AFTER UPDATE OF last_at,due_at ON learning BEGIN
  UPDATE daily_clock SET sequence=sequence+1;
  UPDATE prepared_queue SET due_at=new.due_at,schedule_version=(SELECT sequence FROM daily_clock)
    WHERE card_id=new.card_id;
  DELETE FROM daily_due WHERE card_id=new.card_id AND day=(SELECT current_day FROM daily_clock)
    AND generation IN ((SELECT due_generation FROM daily_clock),
      (SELECT due_head FROM daily_day WHERE day=(SELECT current_day FROM daily_clock)));
END;
CREATE TRIGGER prepare_due_delete AFTER DELETE ON learning BEGIN
  UPDATE daily_clock SET sequence=sequence+1,
    openings=openings+EXISTS(SELECT 1 FROM prepared_queue WHERE card_id=old.card_id AND kind='opening'),
    tactics=tactics+EXISTS(SELECT 1 FROM prepared_queue WHERE card_id=old.card_id AND kind='tactic');
  UPDATE prepared_queue SET due_at=NULL,schedule_version=(SELECT sequence FROM daily_clock)
    WHERE card_id=old.card_id;
  DELETE FROM daily_due WHERE card_id=old.card_id AND day=(SELECT current_day FROM daily_clock)
    AND generation IN ((SELECT due_generation FROM daily_clock),
      (SELECT due_head FROM daily_day WHERE day=(SELECT current_day FROM daily_clock)));
END;
DROP INDEX forecast_new_a;
DROP INDEX forecast_new_b;
DROP INDEX prepared_rank;
DROP INDEX forecast_due_a_0;
DROP INDEX forecast_due_a_2;
DROP INDEX forecast_due_a_4;
DROP INDEX forecast_due_a_6;
DROP INDEX forecast_due_a_8;
DROP INDEX forecast_due_a_10;
DROP INDEX forecast_due_b_0;
DROP INDEX forecast_due_b_2;
DROP INDEX forecast_due_b_4;
DROP INDEX forecast_due_b_6;
DROP INDEX forecast_due_b_8;
DROP INDEX forecast_due_b_10;
