CREATE TABLE coverage_revision (
 repertoire TEXT PRIMARY KEY, revision INTEGER NOT NULL
) STRICT;
CREATE TRIGGER coverage_card_insert AFTER INSERT ON cards WHEN new.kind='opening' BEGIN
 INSERT INTO coverage_revision VALUES(new.repertoire_id,1)
 ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER coverage_card_delete AFTER DELETE ON cards WHEN old.kind='opening' BEGIN
 INSERT INTO coverage_revision VALUES(old.repertoire_id,1)
 ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;
CREATE TRIGGER coverage_card_update AFTER UPDATE OF quarantined,answers,answer_version ON cards WHEN new.kind='opening' BEGIN
 INSERT INTO coverage_revision VALUES(new.repertoire_id,1)
 ON CONFLICT(repertoire) DO UPDATE SET revision=revision+1;
END;
CREATE TABLE coverage_report(id TEXT PRIMARY KEY, repertoire TEXT NOT NULL, revision INTEGER NOT NULL, request TEXT NOT NULL, report TEXT NOT NULL) STRICT;
CREATE TABLE gap_save_receipt(command TEXT PRIMARY KEY, reply TEXT NOT NULL) STRICT;
