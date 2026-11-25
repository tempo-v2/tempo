CREATE INDEX jobs_active_user ON jobs(status,provider,operation,username,id)
WHERE parent_id IS NULL AND operation!='publish' AND status IN ('queued','running');

PRAGMA user_version=6;
