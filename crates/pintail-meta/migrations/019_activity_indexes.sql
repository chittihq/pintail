-- A replication cycle writes one sync_runs row every supervisor cadence -
-- 17,280 a day at the 5-second default - and a dead letter arrives per
-- undecodable event. Nothing prunes either table, and the dashboard reads
-- both as "the most recent N, newest first". Without an index on the sort
-- column SQLite scans and sorts the entire table to return ten rows, so
-- every dashboard load gets slower for the life of the deployment:
-- measured at 145ms over 300,000 rows, which is about seventeen days of
-- uptime, and it grows linearly from there.
--
-- Each table gets two indexes because the reads come in two shapes: scoped
-- to one database, and across all of them.
CREATE INDEX IF NOT EXISTS idx_sync_runs_db_started
    ON sync_runs(db_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_sync_runs_started
    ON sync_runs(started_at DESC);

CREATE INDEX IF NOT EXISTS idx_dlq_db_created
    ON dlq(db_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_dlq_created
    ON dlq(created_at DESC);

PRAGMA user_version = 19;
