ALTER TABLE jobs ADD COLUMN ready_order INTEGER NOT NULL DEFAULT 0;
UPDATE jobs SET ready_order=id WHERE status IN ('queued','running');
CREATE INDEX jobs_ready_order ON jobs(ready_order,id) WHERE status='queued';
CREATE TABLE job_ready_clock (
    singleton INTEGER PRIMARY KEY CHECK (singleton=1),
    last_order INTEGER NOT NULL
) STRICT;
INSERT INTO job_ready_clock SELECT 1, coalesce(max(id),0) FROM jobs;

CREATE TRIGGER jobs_insert_ready AFTER INSERT ON jobs WHEN new.status='queued' BEGIN
    UPDATE job_ready_clock SET last_order=last_order+1 WHERE singleton=1;
    UPDATE jobs SET ready_order=(SELECT last_order FROM job_ready_clock WHERE singleton=1)
    WHERE id=new.id;
END;

CREATE TRIGGER jobs_requeue AFTER UPDATE OF status ON jobs
WHEN new.status='queued' AND old.status NOT IN ('queued','running') BEGIN
    UPDATE job_ready_clock SET last_order=last_order+1 WHERE singleton=1;
    UPDATE jobs SET ready_order=(SELECT last_order FROM job_ready_clock WHERE singleton=1)
    WHERE id=new.id;
END;

PRAGMA user_version=4;
