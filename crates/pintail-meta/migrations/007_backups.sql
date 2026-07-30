CREATE TABLE backup_configs (
    db_id                         TEXT PRIMARY KEY
                                  REFERENCES databases(id) ON DELETE CASCADE,
    bucket                        TEXT NOT NULL,
    prefix                        TEXT NOT NULL,
    endpoint                      TEXT,
    region                        TEXT NOT NULL,
    access_key_id_encrypted       BLOB,
    secret_access_key_encrypted   BLOB,
    schedule_minutes              INTEGER NOT NULL DEFAULT 1440
                                  CHECK (schedule_minutes > 0),
    enabled                       INTEGER NOT NULL DEFAULT 1
                                  CHECK (enabled IN (0, 1)),
    updated_at                    TEXT NOT NULL,
    CHECK (
        (access_key_id_encrypted IS NULL AND secret_access_key_encrypted IS NULL)
        OR
        (access_key_id_encrypted IS NOT NULL AND secret_access_key_encrypted IS NOT NULL)
    )
);

CREATE TABLE backups (
    id              TEXT PRIMARY KEY,
    db_id           TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK (kind IN ('full', 'incremental')),
    parent_id       TEXT REFERENCES backups(id),
    object_prefix   TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('running', 'completed', 'error')),
    bytes           INTEGER NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    object_count    INTEGER NOT NULL DEFAULT 0 CHECK (object_count >= 0),
    error           TEXT,
    started_at      TEXT NOT NULL,
    completed_at    TEXT
);

CREATE INDEX backups_database_started
    ON backups(db_id, started_at DESC);

PRAGMA user_version = 7;
