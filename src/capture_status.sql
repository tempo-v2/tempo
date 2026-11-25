CREATE TABLE capture_status (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
 kept INTEGER NOT NULL, narrowed INTEGER NOT NULL, published INTEGER NOT NULL,
 removed INTEGER NOT NULL, rejected_unpublished INTEGER NOT NULL,
 unresolved INTEGER NOT NULL, retrying INTEGER NOT NULL
) STRICT;

INSERT INTO capture_status
SELECT 1, total(outcome='kept'), total(outcome='narrowed'), total(outcome='published'),
 total(rejected AND existing), total(rejected AND NOT existing),
 total(outcome IS NULL), total(last_error IS NOT NULL)
FROM (
 SELECT outcome, last_error,
   outcome IN ('equivalent','best_outside_candidates','played_best') AS rejected,
   EXISTS(SELECT 1 FROM tactics WHERE game_id=q.game_id AND game_ply=q.ply) AS existing
 FROM capture_qualification q
);

CREATE TRIGGER capture_status_insert AFTER INSERT ON capture_qualification BEGIN
 UPDATE capture_status SET unresolved=unresolved+1 WHERE singleton=1;
END;

-- Terminal verdicts are immutable. The catalog retains a retired tactic's identity.
CREATE TRIGGER capture_status_transition AFTER UPDATE OF outcome,last_error ON capture_qualification
WHEN old.outcome IS NULL BEGIN
 UPDATE capture_status SET
   kept=kept+(new.outcome IS 'kept'),
   narrowed=narrowed+(new.outcome IS 'narrowed'),
   published=published+(new.outcome IS 'published'),
   removed=removed+CASE WHEN new.outcome IN ('equivalent','best_outside_candidates','played_best')
     THEN EXISTS(SELECT 1 FROM tactics WHERE game_id=new.game_id AND game_ply=new.ply) ELSE 0 END,
   rejected_unpublished=rejected_unpublished+CASE WHEN new.outcome IN ('equivalent','best_outside_candidates','played_best')
     THEN NOT EXISTS(SELECT 1 FROM tactics WHERE game_id=new.game_id AND game_ply=new.ply) ELSE 0 END,
   unresolved=unresolved-(new.outcome IS NOT NULL),
   retrying=retrying+(new.last_error IS NOT NULL)-(old.last_error IS NOT NULL)
 WHERE singleton=1;
END;
