-- Keep the old row, labels and revision intact. An alias no longer contributes
-- a separate authored route, and follows its representative's archive state.
WITH representatives AS (
  SELECT id, first_value(id) OVER (
    PARTITION BY repertoire_id,color,line_key
    ORDER BY archived,id<>repertoire_id,rowid
  ) AS representative
  FROM openings
)
UPDATE openings SET alias_of=representative
FROM representatives AS r
WHERE openings.id=r.id AND r.id<>r.representative;

CREATE UNIQUE INDEX opening_line_identity ON openings(repertoire_id,color,line_key)
  WHERE alias_of IS NULL;
CREATE UNIQUE INDEX named_opening_root ON openings(name,color,line_key)
  WHERE id=repertoire_id;

DROP INDEX active_opening_lines;
CREATE INDEX active_opening_lines ON openings(repertoire_id,color,id)
  WHERE archived=0 AND alias_of IS NULL;
CREATE INDEX active_openings_by_id ON openings(id)
  WHERE archived=0 AND alias_of IS NULL;

CREATE VIEW authored_openings AS
SELECT id,repertoire_id,name,color,coalesce(canonical_route,route) AS route,start_at
FROM openings WHERE archived=0 AND alias_of IS NULL;
