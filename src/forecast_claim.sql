CREATE TABLE forecast_claim (
    day TEXT PRIMARY KEY REFERENCES daily_day(day) ON DELETE CASCADE,
    ticket TEXT NOT NULL UNIQUE,
    dataset TEXT NOT NULL,
    authoring INTEGER NOT NULL,
    profile TEXT NOT NULL,
    saved_order_day TEXT,
    expires INTEGER NOT NULL,
    digest TEXT,
    apply_expires INTEGER,
    outcome TEXT CHECK(outcome IN ('published', 'without_rank'))
) STRICT;

-- The stopped predecessor cannot deliver its old private file-format result.
UPDATE daily_day SET forecast_finished=1
WHERE day=(SELECT current_day FROM daily_clock) AND fresh_order=0;
