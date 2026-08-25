-- Local (Pintail-owned, writable) tables need two states a replicated table
-- never has: 'creating', while the store directory and empty manifest are
-- being published, and 'ready', once the table is visible to queries. The
-- catalog and the data live in different durability domains, so CREATE TABLE
-- writes the row first and flips it last; recovery deletes whatever is still
-- 'creating' (docs/design/writable-mode.md).
CREATE TABLE tables_v18 (
    db_id               TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    name                TEXT NOT NULL,
    state               TEXT NOT NULL CHECK (
        state IN (
            'pending', 'snapshotting', 'streaming', 'polling',
            'needs_resync', 'error', 'excluded', 'restored',
            'creating', 'ready'
        )
    ),
    pk_json             TEXT,
    cursor_column       TEXT,
    sort_key_json       TEXT,
    rows_synced         INTEGER NOT NULL DEFAULT 0 CHECK (rows_synced >= 0),
    last_error          TEXT,
    last_reconcile_at   TEXT,
    schema_version      INTEGER NOT NULL DEFAULT 1 CHECK (schema_version > 0),
    orphaned_at         TEXT,
    soft_delete_column  TEXT,
    PRIMARY KEY (db_id, name)
);

INSERT INTO tables_v18 (
    db_id, name, state, pk_json, cursor_column, sort_key_json,
    rows_synced, last_error, last_reconcile_at, schema_version,
    orphaned_at, soft_delete_column
)
SELECT
    db_id, name, state, pk_json, cursor_column, sort_key_json,
    rows_synced, last_error, last_reconcile_at, schema_version,
    orphaned_at, soft_delete_column
FROM tables;

DROP TABLE tables;
ALTER TABLE tables_v18 RENAME TO tables;

PRAGMA user_version = 18;
