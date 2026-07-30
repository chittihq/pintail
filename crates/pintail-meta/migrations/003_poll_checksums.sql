CREATE TABLE poll_chunk_states (
    db_id               TEXT NOT NULL,
    table_name          TEXT NOT NULL,
    chunk_id            TEXT NOT NULL,
    source_count        INTEGER NOT NULL CHECK (source_count >= 0),
    source_checksum     TEXT NOT NULL,
    replica_checksum    TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    PRIMARY KEY (db_id, table_name, chunk_id),
    FOREIGN KEY (db_id, table_name)
        REFERENCES tables(db_id, name) ON DELETE CASCADE
);

PRAGMA user_version = 3;
