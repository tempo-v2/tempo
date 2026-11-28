-- Capture order is assigned by the catalog writer to an unseen authored card.
ALTER TABLE cards ADD COLUMN capture_order INTEGER CHECK (capture_order IS NULL OR (kind='tactic' AND capture_order>0));
ALTER TABLE prepared_queue ADD COLUMN capture_order INTEGER;
CREATE INDEX prepared_capture_order ON prepared_queue(capture_order,card_id)
  WHERE kind='tactic' AND active=1 AND reserved=0 AND due_at IS NULL AND capture_order IS NOT NULL;
CREATE INDEX prepared_ordinary_tactic ON prepared_queue(card_id)
  WHERE kind='tactic' AND active=1 AND reserved=0 AND due_at IS NULL AND capture_order IS NULL;

CREATE TRIGGER project_tactic_capture AFTER UPDATE OF capture_order ON cards
WHEN new.capture_order IS NOT old.capture_order BEGIN
  UPDATE daily_clock SET sequence=new.capture_order,tactics=tactics+1;
  UPDATE prepared_queue SET capture_order=new.capture_order WHERE card_id=new.id;
END;

CREATE TABLE tactic_capture_receipts (
  command TEXT PRIMARY KEY,
  fingerprint TEXT NOT NULL,
  card_id TEXT NOT NULL REFERENCES cards(id),
  reply TEXT NOT NULL CHECK(json_valid(reply))
) STRICT;
