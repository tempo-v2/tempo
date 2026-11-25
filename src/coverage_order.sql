ALTER TABLE prepared_queue ADD COLUMN forecast_group TEXT;
DROP TRIGGER prepare_card;
CREATE TRIGGER prepare_card AFTER INSERT ON cards BEGIN
  INSERT INTO prepared_queue(card_id,kind,active,due_at,rank,reserved)
  VALUES (new.id,new.kind,NOT new.quarantined,NULL,NULL,0);
END;
CREATE INDEX prepared_forecast_due ON prepared_queue(forecast_group,rank,card_id,due_at)
 WHERE active=1 AND reserved=0 AND due_at IS NOT NULL AND forecast_group IS NOT NULL;
CREATE TABLE applied_forecast(command TEXT PRIMARY KEY,report TEXT NOT NULL REFERENCES coverage_report(id)) STRICT;
