CREATE TABLE prepared_queue_state (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  ready INTEGER NOT NULL CHECK (ready IN (0,1)),
  cursor TEXT NOT NULL
) STRICT;
INSERT INTO prepared_queue_state VALUES (1,0,'');

CREATE TABLE prepared_queue (
  card_id TEXT PRIMARY KEY REFERENCES cards(id),
  kind TEXT NOT NULL,
  active INTEGER NOT NULL,
  due_at INTEGER,
  rank INTEGER,
  reserved INTEGER NOT NULL
) STRICT;
CREATE INDEX prepared_due ON prepared_queue(kind,due_at,card_id)
  WHERE active=1 AND reserved=0 AND due_at IS NOT NULL;
CREATE INDEX prepared_unseen ON prepared_queue(kind,card_id)
  WHERE active=1 AND reserved=0 AND due_at IS NULL;
CREATE INDEX prepared_rank ON prepared_queue(kind,rank,card_id)
  WHERE active=1 AND reserved=0 AND due_at IS NULL AND rank IS NOT NULL;

CREATE TRIGGER prepare_card AFTER INSERT ON cards BEGIN
  INSERT INTO prepared_queue VALUES (new.id,new.kind,NOT new.quarantined,NULL,NULL,0);
END;
CREATE TRIGGER prepare_quarantine AFTER UPDATE OF quarantined ON cards BEGIN
  UPDATE prepared_queue SET active=NOT new.quarantined WHERE card_id=new.id;
END;
CREATE TRIGGER prepare_due_insert AFTER INSERT ON learning BEGIN
  UPDATE prepared_queue SET due_at=new.due_at WHERE card_id=new.card_id;
END;
CREATE TRIGGER prepare_due_update AFTER UPDATE OF due_at ON learning BEGIN
  UPDATE prepared_queue SET due_at=new.due_at WHERE card_id=new.card_id;
END;
CREATE TRIGGER prepare_due_delete AFTER DELETE ON learning BEGIN
  UPDATE prepared_queue SET due_at=NULL WHERE card_id=old.card_id;
END;
CREATE TRIGGER prepare_rank_insert AFTER INSERT ON opening_order BEGIN
  UPDATE prepared_queue SET rank=new.rank WHERE card_id=new.card_id;
END;
CREATE TRIGGER prepare_rank_delete AFTER DELETE ON opening_order BEGIN
  UPDATE prepared_queue SET rank=NULL WHERE card_id=old.card_id;
END;
CREATE TRIGGER prepare_reserve AFTER INSERT ON deck BEGIN
  UPDATE prepared_queue SET reserved=1 WHERE card_id=new.card_id;
END;
CREATE TRIGGER prepare_release AFTER DELETE ON deck BEGIN
  UPDATE prepared_queue SET reserved=0 WHERE card_id=old.card_id;
END;

CREATE TABLE repeat_obligation (
  attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
  card_id TEXT NOT NULL REFERENCES cards(id),
  previous_kind TEXT,
  accepted_at INTEGER NOT NULL,
  draw INTEGER NOT NULL CHECK(draw BETWEEN 5 AND 20)
) STRICT;

ALTER TABLE jobs ADD COLUMN upload_file TEXT;
CREATE INDEX jobs_upload ON jobs(upload_file) WHERE upload_file IS NOT NULL;

PRAGMA user_version=3;
