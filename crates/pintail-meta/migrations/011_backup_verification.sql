-- Restore validation: optionally restore each completed backup into a
-- scratch directory (download + checksum every object) and record the
-- outcome on the backup row.
ALTER TABLE backup_configs ADD COLUMN verify_restore INTEGER NOT NULL DEFAULT 0;
ALTER TABLE backups ADD COLUMN verified_at TEXT NULL;
ALTER TABLE backups ADD COLUMN verify_error TEXT NULL;
PRAGMA user_version = 11;
