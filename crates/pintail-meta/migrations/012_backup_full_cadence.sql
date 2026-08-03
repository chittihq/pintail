-- Force a full backup every Nth scheduled run. Zero (the default) keeps
-- the previous behavior: one full, then incrementals forever.
ALTER TABLE backup_configs ADD COLUMN full_every INTEGER NOT NULL DEFAULT 0;
PRAGMA user_version = 12;
