-- The audit log is pruned by age across every workspace. Its only index
-- leads with the workspace, so finding the oldest rows meant reading the
-- whole table on every pruning batch; this index makes each batch read only
-- the rows it deletes.
CREATE INDEX IF NOT EXISTS idx_audit_log_created ON audit_log(created_at);

PRAGMA user_version = 25;
