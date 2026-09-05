-- Whether a table's copy reached its end. The table state cannot say: a
-- snapshot job walks tables through snapshotting -> pending -> streaming, a
-- resync resets one table to snapshotting, and a failed job marks every
-- table error. A restart in the middle of any of those left a database with
-- healthy, fully copied tables it could no longer tell from half-copied
-- ones, and the only safe answer was to walk them all again - re-reading
-- the whole source before reaching the single table that needed a copy.
-- The marker is set when a table's copy completes or its resync
-- finishes, and cleared when a copy or resync begins or the table is
-- flagged for one.
ALTER TABLE tables ADD COLUMN copy_complete INTEGER NOT NULL DEFAULT 0;

-- Tables handed off (streaming, polling) or copied and awaiting handoff
-- (pending) hold a complete copy.
UPDATE tables SET copy_complete = 1
    WHERE state IN ('streaming', 'polling', 'pending');

-- A table the restart walk refused with "requires an empty memtable" holds
-- one as well: only a table already live under replication has rows in its
-- memtable, and the walk failed before writing anything. The same text is
-- what a failed job wrote into every other table; rows_synced separates the
-- copied ones from a table whose copy had not started.
UPDATE tables SET copy_complete = 1
    WHERE state = 'error'
      AND rows_synced > 0
      AND last_error LIKE '%direct snapshot ingest requires an empty memtable%';

PRAGMA user_version = 20;
