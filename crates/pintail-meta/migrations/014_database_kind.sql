ALTER TABLE databases
ADD COLUMN kind TEXT NOT NULL DEFAULT 'replicated';

PRAGMA user_version = 14;
