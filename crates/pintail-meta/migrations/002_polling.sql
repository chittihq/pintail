CREATE TABLE poll_states (
    db_id               TEXT NOT NULL,
    table_name          TEXT NOT NULL,
    cursor_column       TEXT,
    cursor_json         TEXT,
    source_token_json   TEXT,
    source_count        INTEGER NOT NULL DEFAULT 0 CHECK (source_count >= 0),
    version             INTEGER NOT NULL DEFAULT 0 CHECK (version >= 0),
    last_reconcile_at   TEXT,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (db_id, table_name),
    FOREIGN KEY (db_id, table_name)
        REFERENCES tables(db_id, name) ON DELETE CASCADE
);

PRAGMA user_version = 2;
