ALTER TABLE users
ADD COLUMN google_subject TEXT;

CREATE UNIQUE INDEX idx_users_google_subject ON users(google_subject)
WHERE google_subject IS NOT NULL;

CREATE TABLE invites (
    id             TEXT PRIMARY KEY,
    token_hash     BLOB NOT NULL UNIQUE,
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    email          TEXT NOT NULL,
    role           TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    created_by     TEXT NOT NULL REFERENCES users(id),
    created_at     TEXT NOT NULL,
    expires_at     TEXT NOT NULL,
    accepted_at    TEXT,
    revoked_at     TEXT
);

CREATE INDEX idx_invites_workspace ON invites(workspace_id);
CREATE INDEX idx_invites_email ON invites(email);

CREATE TABLE audit_log (
    id             TEXT PRIMARY KEY,
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    actor_type     TEXT NOT NULL CHECK (actor_type IN ('user', 'api_key')),
    actor_id       TEXT NOT NULL,
    actor_label    TEXT NOT NULL,
    action         TEXT NOT NULL,
    target_type    TEXT,
    target_id      TEXT,
    detail_json    TEXT,
    created_at     TEXT NOT NULL
);

CREATE INDEX idx_audit_log_workspace_created ON audit_log(workspace_id, created_at DESC);

PRAGMA user_version = 16;
