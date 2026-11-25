CREATE TABLE gap_batch (
    command TEXT PRIMARY KEY,
    revision INTEGER NOT NULL,
    line INTEGER NOT NULL CHECK(line >= 0),
    card INTEGER NOT NULL CHECK(card >= 0),
    total INTEGER NOT NULL CHECK(total > 0)
) STRICT;
