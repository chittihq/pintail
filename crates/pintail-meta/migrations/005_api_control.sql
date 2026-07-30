ALTER TABLE users
ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1));

ALTER TABLE users
ADD COLUMN last_login_at TEXT;

ALTER TABLE api_keys
ADD COLUMN scopes_json TEXT NOT NULL DEFAULT '["query","read"]';

ALTER TABLE databases
ADD COLUMN poll_interval_seconds INTEGER NOT NULL DEFAULT 5
CHECK (poll_interval_seconds > 0);

ALTER TABLE databases
ADD COLUMN reconcile_interval_seconds INTEGER NOT NULL DEFAULT 600
CHECK (reconcile_interval_seconds > 0);

ALTER TABLE tables
ADD COLUMN soft_delete_column TEXT;

PRAGMA user_version = 5;
