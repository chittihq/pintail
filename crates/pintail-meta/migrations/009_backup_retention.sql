-- Retention: how many completed backups to keep per database. Zero keeps
-- everything (the previous behavior).
ALTER TABLE backup_configs ADD COLUMN retain_count INTEGER NOT NULL DEFAULT 0;
PRAGMA user_version = 9;
