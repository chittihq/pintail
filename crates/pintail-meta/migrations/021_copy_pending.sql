-- A started copy must finish even when keyless mutations normally require
-- operator repair. Keep that intent across errors independently of readiness.
ALTER TABLE tables ADD COLUMN copy_pending INTEGER NOT NULL DEFAULT 0;
UPDATE tables SET copy_pending = 1
    WHERE copy_complete = 0 AND state = 'snapshotting';
PRAGMA user_version = 21;
