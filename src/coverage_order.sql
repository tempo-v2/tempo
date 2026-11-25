-- Stage rank changes in the inactive columns and publish with one pointer flip.
CREATE TABLE forecast_order_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1), bank INTEGER NOT NULL CHECK(bank IN (0,1))) STRICT;
INSERT INTO forecast_order_state VALUES(1,0);
CREATE TABLE applied_forecast(command TEXT PRIMARY KEY, report TEXT NOT NULL REFERENCES coverage_report(id)) STRICT;
DROP TRIGGER prepare_card;
CREATE TRIGGER prepare_card AFTER INSERT ON cards BEGIN
  INSERT INTO prepared_queue(card_id,kind,active,due_at,rank,reserved)
  VALUES (new.id,new.kind,NOT new.quarantined,NULL,NULL,0);
END;
ALTER TABLE prepared_queue ADD COLUMN rank_a INTEGER;
ALTER TABLE prepared_queue ADD COLUMN scope_a TEXT;
ALTER TABLE prepared_queue ADD COLUMN slot_a INTEGER;
CREATE INDEX forecast_new_a ON prepared_queue(kind,coalesce(rank_a,rank),card_id)
 WHERE active=1 AND reserved=0 AND due_at IS NULL AND coalesce(rank_a,rank) IS NOT NULL;
CREATE INDEX forecast_due_a_10 ON prepared_queue(scope_a,(slot_a >> 10),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_a IS NOT NULL;
CREATE INDEX forecast_due_a_8 ON prepared_queue(scope_a,(slot_a >> 8),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_a IS NOT NULL;
CREATE INDEX forecast_due_a_6 ON prepared_queue(scope_a,(slot_a >> 6),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_a IS NOT NULL;
CREATE INDEX forecast_due_a_4 ON prepared_queue(scope_a,(slot_a >> 4),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_a IS NOT NULL;
CREATE INDEX forecast_due_a_2 ON prepared_queue(scope_a,(slot_a >> 2),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_a IS NOT NULL;
CREATE INDEX forecast_due_a_0 ON prepared_queue(scope_a,(slot_a >> 0),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_a IS NOT NULL;
ALTER TABLE prepared_queue ADD COLUMN rank_b INTEGER;
ALTER TABLE prepared_queue ADD COLUMN scope_b TEXT;
ALTER TABLE prepared_queue ADD COLUMN slot_b INTEGER;
CREATE INDEX forecast_new_b ON prepared_queue(kind,coalesce(rank_b,rank),card_id)
 WHERE active=1 AND reserved=0 AND due_at IS NULL AND coalesce(rank_b,rank) IS NOT NULL;
CREATE INDEX forecast_due_b_10 ON prepared_queue(scope_b,(slot_b >> 10),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_b IS NOT NULL;
CREATE INDEX forecast_due_b_8 ON prepared_queue(scope_b,(slot_b >> 8),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_b IS NOT NULL;
CREATE INDEX forecast_due_b_6 ON prepared_queue(scope_b,(slot_b >> 6),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_b IS NOT NULL;
CREATE INDEX forecast_due_b_4 ON prepared_queue(scope_b,(slot_b >> 4),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_b IS NOT NULL;
CREATE INDEX forecast_due_b_2 ON prepared_queue(scope_b,(slot_b >> 2),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_b IS NOT NULL;
CREATE INDEX forecast_due_b_0 ON prepared_queue(scope_b,(slot_b >> 0),due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND scope_b IS NOT NULL;
