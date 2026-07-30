ALTER TABLE tables ADD COLUMN orphaned_at TEXT;

PRAGMA user_version = 4;
