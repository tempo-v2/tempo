CREATE TABLE lichess_catalog (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1), artifact TEXT NOT NULL, source TEXT NOT NULL
) STRICT;
CREATE TABLE lichess_versions (
  puzzle TEXT NOT NULL, version TEXT NOT NULL, fen TEXT NOT NULL, moves TEXT NOT NULL,
  PRIMARY KEY(puzzle,version)
) STRICT;
CREATE TABLE lichess_packs (
  theme TEXT NOT NULL, band INTEGER NOT NULL, ceiling INTEGER,
  PRIMARY KEY(theme,band)
) STRICT;
CREATE TABLE lichess_members (
  theme TEXT NOT NULL, band INTEGER NOT NULL, position INTEGER NOT NULL CHECK(position BETWEEN 1 AND 20),
  puzzle TEXT NOT NULL, version TEXT NOT NULL, rating INTEGER NOT NULL,
  PRIMARY KEY(theme,band,position), UNIQUE(theme,puzzle),
  FOREIGN KEY(theme,band) REFERENCES lichess_packs(theme,band) ON DELETE CASCADE,
  FOREIGN KEY(puzzle,version) REFERENCES lichess_versions(puzzle,version)
) STRICT;
CREATE INDEX lichess_members_puzzle ON lichess_members(puzzle,version);
CREATE TABLE lichess_cards (
  puzzle TEXT PRIMARY KEY, version TEXT NOT NULL, card TEXT NOT NULL UNIQUE REFERENCES cards(id),
  FOREIGN KEY(puzzle,version) REFERENCES lichess_versions(puzzle,version)
) STRICT;
CREATE TABLE lichess_plays (
  id INTEGER PRIMARY KEY, puzzle TEXT NOT NULL, version TEXT NOT NULL,
  mode TEXT NOT NULL CHECK(mode IN ('first','practice','study')),
  review INTEGER UNIQUE REFERENCES attempts(id), ply INTEGER NOT NULL DEFAULT 0,
  result TEXT, final_move TEXT,
  FOREIGN KEY(puzzle,version) REFERENCES lichess_versions(puzzle,version),
  CHECK(mode!='study' OR review IS NOT NULL)
) STRICT;
CREATE UNIQUE INDEX lichess_first_play ON lichess_plays(puzzle) WHERE mode='first';
CREATE INDEX lichess_pending_first ON lichess_plays(id) WHERE mode='first' AND result IS NULL;
CREATE TABLE lichess_starts (
  request TEXT PRIMARY KEY, puzzle TEXT NOT NULL, practice INTEGER NOT NULL,
  play INTEGER NOT NULL REFERENCES lichess_plays(id)
) STRICT;
CREATE TABLE lichess_moves (
  play INTEGER NOT NULL REFERENCES lichess_plays(id), step INTEGER NOT NULL,
  action TEXT NOT NULL, reply TEXT NOT NULL, PRIMARY KEY(play,step)
) STRICT;
CREATE INDEX repeat_by_card ON repeat_obligation(card_id);
CREATE TABLE repeat_order (
  seq INTEGER PRIMARY KEY, attempt_id INTEGER NOT NULL UNIQUE REFERENCES repeat_obligation(attempt_id) ON DELETE CASCADE,
  card_id TEXT NOT NULL REFERENCES cards(id)
) STRICT;
INSERT INTO repeat_order(attempt_id,card_id) SELECT attempt_id,card_id FROM repeat_obligation ORDER BY accepted_at,attempt_id;
CREATE TRIGGER reserve_pending_repeat AFTER INSERT ON repeat_obligation BEGIN
  INSERT INTO repeat_order(attempt_id,card_id) VALUES (new.attempt_id,new.card_id);
  UPDATE prepared_queue SET reserved=1 WHERE card_id=new.card_id;
END;
CREATE TRIGGER release_pending_repeat AFTER DELETE ON repeat_obligation BEGIN
  UPDATE prepared_queue SET reserved=EXISTS(SELECT 1 FROM deck WHERE card_id=old.card_id)
    WHERE card_id=old.card_id;
END;
UPDATE prepared_queue SET reserved=1 WHERE card_id IN (SELECT card_id FROM repeat_obligation);
