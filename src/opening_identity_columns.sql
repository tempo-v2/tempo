ALTER TABLE openings ADD COLUMN line_key TEXT;
ALTER TABLE openings ADD COLUMN canonical_route TEXT;
ALTER TABLE openings ADD COLUMN alias_of TEXT REFERENCES openings(id) CHECK(alias_of<>id);
ALTER TABLE gap_batch ADD COLUMN resolved_lines TEXT;
