ALTER TABLE openings ADD COLUMN line_key TEXT;
ALTER TABLE openings ADD COLUMN canonical_route TEXT;
ALTER TABLE openings ADD COLUMN alias_of TEXT REFERENCES openings(id) CHECK(alias_of<>id);
ALTER TABLE openings ADD COLUMN pre_schema18_archived INTEGER CHECK(pre_schema18_archived IN (0,1));
ALTER TABLE gap_batch ADD COLUMN resolved_lines TEXT;
