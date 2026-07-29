CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    argon2_hash   TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    created_at    TEXT NOT NULL
);

CREATE TABLE databases (
    id                    TEXT PRIMARY KEY,
    name                  TEXT NOT NULL UNIQUE,
    mysql_dsn_encrypted   BLOB NOT NULL,
    mode                  TEXT NOT NULL CHECK (mode IN ('auto', 'cdc', 'polling', 'paused')),
    effective_mode        TEXT CHECK (effective_mode IN ('cdc', 'polling', 'paused')),
    state                 TEXT NOT NULL,
    probe_json            TEXT,
    include_tables        TEXT,
    exclude_tables        TEXT,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL
);

CREATE TABLE tables (
    db_id               TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    name                TEXT NOT NULL,
    state               TEXT NOT NULL CHECK (
        state IN (
            'pending', 'snapshotting', 'streaming', 'polling',
            'needs_resync', 'error', 'excluded'
        )
    ),
    pk_json             TEXT,
    cursor_column       TEXT,
    sort_key_json       TEXT,
    rows_synced         INTEGER NOT NULL DEFAULT 0 CHECK (rows_synced >= 0),
    last_error          TEXT,
    last_reconcile_at   TEXT,
    schema_version      INTEGER NOT NULL DEFAULT 1 CHECK (schema_version > 0),
    PRIMARY KEY (db_id, name)
);

CREATE TABLE checkpoints (
    db_id                TEXT PRIMARY KEY REFERENCES databases(id) ON DELETE CASCADE,
    kind                 TEXT NOT NULL CHECK (kind IN ('gtid', 'filepos', 'polling')),
    gtid_set             TEXT,
    binlog_file          TEXT,
    binlog_pos           INTEGER,
    poll_cursors_json    TEXT,
    updated_at           TEXT NOT NULL
);

CREATE TABLE snapshot_chunks (
    db_id          TEXT NOT NULL,
    table_name     TEXT NOT NULL,
    chunk_id       TEXT NOT NULL,
    lo_key_json    TEXT,
    hi_key_json    TEXT,
    status         TEXT NOT NULL CHECK (
        status IN ('pending', 'running', 'completed', 'error')
    ),
    rows           INTEGER NOT NULL DEFAULT 0 CHECK (rows >= 0),
    PRIMARY KEY (db_id, table_name, chunk_id),
    FOREIGN KEY (db_id, table_name)
        REFERENCES tables(db_id, name) ON DELETE CASCADE
);

CREATE TABLE schema_history (
    db_id          TEXT NOT NULL,
    table_name     TEXT NOT NULL,
    version        INTEGER NOT NULL CHECK (version > 0),
    ddl_text       TEXT,
    columns_json   TEXT NOT NULL,
    applied_at     TEXT NOT NULL,
    PRIMARY KEY (db_id, table_name, version),
    FOREIGN KEY (db_id, table_name)
        REFERENCES tables(db_id, name) ON DELETE CASCADE
);

CREATE TABLE api_keys (
    id            TEXT PRIMARY KEY,
    db_id         TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    sha256        BLOB NOT NULL UNIQUE,
    enabled       INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    expires_at    TEXT,
    last_used_at  TEXT,
    created_at    TEXT NOT NULL
);

CREATE TABLE sync_runs (
    id            TEXT PRIMARY KEY,
    db_id         TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    table_name    TEXT,
    kind          TEXT NOT NULL,
    status        TEXT NOT NULL,
    rows          INTEGER NOT NULL DEFAULT 0 CHECK (rows >= 0),
    bytes         INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    duration_ms   INTEGER,
    error         TEXT,
    started_at    TEXT NOT NULL
);

CREATE TABLE dlq (
    id            TEXT PRIMARY KEY,
    db_id         TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    table_name    TEXT,
    event_json    TEXT NOT NULL,
    error         TEXT NOT NULL,
    created_at    TEXT NOT NULL
);

CREATE TABLE settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

PRAGMA user_version = 1;
