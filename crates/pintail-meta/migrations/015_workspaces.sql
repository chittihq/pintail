CREATE TABLE workspaces (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    slug         TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL
);

CREATE TABLE workspace_members (
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role           TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    created_at     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, user_id)
);

CREATE INDEX idx_workspace_members_user ON workspace_members(user_id);

ALTER TABLE databases
ADD COLUMN workspace_id TEXT REFERENCES workspaces(id);

CREATE INDEX idx_databases_workspace ON databases(workspace_id);

-- Every node upgrading from a single-workspace install gets one "Default
-- workspace" seeded from its existing users/databases, so nothing already
-- configured becomes orphaned. New nodes past this version start empty; the
-- first user's setup() creates their own first workspace directly.
INSERT INTO workspaces (id, name, slug, created_at)
SELECT 'ws_default', 'Default workspace', 'default', datetime('now')
WHERE EXISTS (SELECT 1 FROM users);

INSERT INTO workspace_members (workspace_id, user_id, role, created_at)
SELECT 'ws_default', id, role, datetime('now') FROM users;

UPDATE databases SET workspace_id = 'ws_default'
WHERE workspace_id IS NULL AND EXISTS (SELECT 1 FROM workspaces WHERE id = 'ws_default');

PRAGMA user_version = 15;
