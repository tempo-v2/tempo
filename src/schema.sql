CREATE TABLE metadata (dataset TEXT PRIMARY KEY) STRICT;
CREATE TABLE openings (
  id TEXT PRIMARY KEY, repertoire_id TEXT NOT NULL, name TEXT NOT NULL,
  color TEXT NOT NULL, route TEXT NOT NULL, start_at INTEGER NOT NULL
) STRICT;
CREATE TABLE tactics (
  id TEXT PRIMARY KEY, name TEXT NOT NULL, route TEXT NOT NULL,
  game_id TEXT, game_ply INTEGER, source_url TEXT
) STRICT;
CREATE TABLE cards (
  id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK (kind IN ('opening','tactic')),
  title TEXT NOT NULL, color TEXT NOT NULL, fen TEXT NOT NULL, position_id TEXT NOT NULL,
  answers TEXT NOT NULL, repertoire_id TEXT, answer_version TEXT,
  played_move TEXT, context TEXT, source_game_id TEXT, source_ply INTEGER
) STRICT;
CREATE INDEX card_position ON cards(position_id,kind,color);
CREATE INDEX card_kind ON cards(kind,id);
CREATE TABLE games (
  id TEXT PRIMARY KEY, white TEXT NOT NULL, black TEXT NOT NULL,
  learner_color TEXT NOT NULL, route TEXT NOT NULL, source_url TEXT, played_on TEXT
) STRICT;
CREATE TABLE mismatches (
  game_id TEXT NOT NULL REFERENCES games(id), ply INTEGER NOT NULL,
  position_id TEXT NOT NULL, repertoire_id TEXT NOT NULL, answer_version TEXT NOT NULL,
  played_move TEXT NOT NULL,
  PRIMARY KEY (game_id,ply,repertoire_id,answer_version)
) STRICT;
CREATE INDEX mismatch_card ON mismatches(position_id,repertoire_id,answer_version);
CREATE TABLE missed_captures (
  game_id TEXT NOT NULL REFERENCES games(id), ply INTEGER NOT NULL, position_id TEXT NOT NULL,
  played_move TEXT NOT NULL, captures TEXT NOT NULL,
  PRIMARY KEY (game_id,ply)
) STRICT;
CREATE TABLE settings (provider TEXT PRIMARY KEY, username TEXT NOT NULL) STRICT;
CREATE TABLE jobs (
  id INTEGER PRIMARY KEY, provider TEXT NOT NULL, username TEXT NOT NULL,
  operation TEXT NOT NULL, payload TEXT, cursor TEXT,
  status TEXT NOT NULL CHECK (status IN ('queued','running','failed','done')),
  added INTEGER NOT NULL DEFAULT 0, existing INTEGER NOT NULL DEFAULT 0,
  rejected INTEGER NOT NULL DEFAULT 0, found INTEGER NOT NULL DEFAULT 0, error TEXT
) STRICT;
CREATE INDEX job_status ON jobs(status,id);
CREATE TABLE provider_checkpoint (provider TEXT NOT NULL, username TEXT NOT NULL,
  through_at INTEGER NOT NULL, PRIMARY KEY(provider,username)) STRICT;
CREATE TABLE learning (
  card_id TEXT PRIMARY KEY REFERENCES cards(id), stability REAL NOT NULL,
  difficulty REAL NOT NULL, last_at INTEGER NOT NULL, due_at INTEGER NOT NULL
) STRICT;
CREATE INDEX learning_due ON learning(due_at,card_id);
CREATE TABLE study (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1), last_kind TEXT, active INTEGER
) STRICT;
INSERT INTO study VALUES (1,NULL,NULL);
CREATE TABLE attempts (
  id INTEGER PRIMARY KEY, card_id TEXT NOT NULL REFERENCES cards(id),
  submission TEXT, result TEXT
) STRICT;
CREATE TABLE deck (
  slot INTEGER PRIMARY KEY, card_id TEXT NOT NULL UNIQUE REFERENCES cards(id)
) STRICT;
CREATE TABLE job_games (
  job_id INTEGER NOT NULL REFERENCES jobs(id), game_id TEXT NOT NULL, scanned INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(job_id,game_id)
) STRICT;
