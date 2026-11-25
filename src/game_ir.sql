CREATE TABLE games_v7 (
    ordinal INTEGER PRIMARY KEY,
    id TEXT UNIQUE NOT NULL,
    white TEXT NOT NULL,
    black TEXT NOT NULL,
    learner_color TEXT NOT NULL CHECK(learner_color IN ('white','black')),
    game_ir BLOB NOT NULL CHECK(length(game_ir) BETWEEN 8 AND 2213),
    source_url TEXT,
    played_on TEXT,
    occurrence TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK(outcome IN ('*','1-0','0-1','1/2-1/2'))
) STRICT;
