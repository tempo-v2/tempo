-- Moving an active route to its saved representative changes no authored
-- route or reply. Preserve the prior archive flag when authority moves.
DROP TRIGGER opening_line_archive;
WITH representatives AS MATERIALIZED (
  SELECT id, first_value(id) OVER group_order AS representative,
         min(archived) OVER (PARTITION BY repertoire_id,color,line_key) AS group_archived
  FROM openings
  WINDOW group_order AS (PARTITION BY repertoire_id,color,line_key ORDER BY id<>repertoire_id,rowid)
)
UPDATE openings SET
  alias_of=CASE WHEN openings.id<>representative THEN representative END,
  pre_schema18_archived=CASE WHEN openings.id=representative AND archived<>group_archived THEN archived END,
  archived=CASE WHEN openings.id=representative THEN group_archived ELSE archived END
FROM representatives AS r
WHERE openings.id=r.id;

CREATE TRIGGER opening_line_archive AFTER UPDATE OF archived ON openings
WHEN new.archived<>old.archived BEGIN
  INSERT INTO opening_line_revision VALUES(new.repertoire_id,1)
  ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;

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
