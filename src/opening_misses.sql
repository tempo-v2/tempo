DROP INDEX repeat_by_card;
CREATE UNIQUE INDEX repeat_by_card ON repeat_obligation(card_id);

DROP TRIGGER prepare_release;
CREATE TRIGGER prepare_release AFTER DELETE ON deck BEGIN
  UPDATE prepared_queue SET reserved=EXISTS(
    SELECT 1 FROM repeat_obligation WHERE card_id=old.card_id
  ) WHERE card_id=old.card_id;
END;

CREATE TRIGGER repeat_requires_unqueued_card BEFORE INSERT ON repeat_obligation
WHEN EXISTS(SELECT 1 FROM deck WHERE card_id=new.card_id) BEGIN
  SELECT raise(ABORT, 'repeat target already in deck');
END;

-- Repair a projection left behind by the former release trigger.
UPDATE prepared_queue SET reserved=(
  EXISTS(SELECT 1 FROM deck WHERE card_id=prepared_queue.card_id)
  OR EXISTS(SELECT 1 FROM repeat_obligation WHERE card_id=prepared_queue.card_id)
) WHERE reserved!=(
  EXISTS(SELECT 1 FROM deck WHERE card_id=prepared_queue.card_id)
  OR EXISTS(SELECT 1 FROM repeat_obligation WHERE card_id=prepared_queue.card_id)
);

CREATE TABLE opening_priority (
  id INTEGER PRIMARY KEY,
  target TEXT NOT NULL REFERENCES cards(id),
  availability INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'preparing'
    CHECK(state IN ('preparing','pending','blocked','invalidated','fulfilled','cancelled')),
  summary TEXT CHECK(length(CAST(summary AS BLOB))<=16384)
) STRICT;
CREATE UNIQUE INDEX opening_priority_active ON opening_priority(target,availability)
  WHERE state IN ('preparing','pending','blocked');
CREATE INDEX opening_priority_sequence ON opening_priority(id)
  WHERE state IN ('preparing','pending','blocked');

-- A first accepted Study introduction can complete a certified route before
-- the worker wakes. Preserve its proof generation across a subsequent edit.
CREATE TABLE opening_priority_introduction (
  card_id TEXT NOT NULL REFERENCES cards(id),
  openings INTEGER NOT NULL,
  schedule_version INTEGER NOT NULL,
  PRIMARY KEY(card_id,openings)
) STRICT;

CREATE TABLE opening_miss_receipts (
  request TEXT PRIMARY KEY,
  event TEXT NOT NULL UNIQUE,
  assessment INTEGER UNIQUE REFERENCES attempts(id),
  goal INTEGER NOT NULL REFERENCES opening_priority(id),
  receipt TEXT NOT NULL CHECK(length(CAST(receipt AS BLOB))<=32768)
) STRICT;

CREATE TABLE opening_miss_requests (
  request TEXT PRIMARY KEY,
  fingerprint TEXT NOT NULL,
  original TEXT NOT NULL REFERENCES opening_miss_receipts(request)
) STRICT;

ALTER TABLE daily_clock ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;

-- The latest compatible daily proof prefix is disposable. Only the goals above are authority.
CREATE TABLE opening_priority_projection (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  day TEXT NOT NULL,
  openings INTEGER NOT NULL,
  priority INTEGER NOT NULL,
  commitments INTEGER NOT NULL,
  paused INTEGER NOT NULL CHECK(paused IN (0,1)),
  prefix TEXT NOT NULL
) STRICT;
