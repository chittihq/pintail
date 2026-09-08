ALTER TABLE databases ADD COLUMN restored_backup_created_at TEXT;
PRAGMA user_version = 23;
