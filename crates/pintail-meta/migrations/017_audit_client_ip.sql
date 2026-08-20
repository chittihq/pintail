-- The audit trail answers "who did what"; for a trail that is worth its
-- name, "from where" belongs beside it. NULL for rows recorded before this
-- migration and for actions with no network peer (supervisor-internal work).
ALTER TABLE audit_log ADD COLUMN client_ip TEXT;

PRAGMA user_version = 17;
