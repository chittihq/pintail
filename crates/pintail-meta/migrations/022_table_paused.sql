-- An operator can hold one table still while the rest of its database
-- keeps replicating: a paused table's row events are passed over and
-- polling leaves it alone until it is resumed. The stream records once
-- that it passed changes over, so resuming knows whether the table needs
-- a recopy or can simply move again.
ALTER TABLE tables ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tables ADD COLUMN paused_skipped INTEGER NOT NULL DEFAULT 0;
PRAGMA user_version = 22;
