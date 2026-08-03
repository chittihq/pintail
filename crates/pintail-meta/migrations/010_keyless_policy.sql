-- Keyless-table replication policy per database:
--   quarantine  — keyless UPDATE/DELETE decode failures flag the table
--                 needs_resync and wait for an operator (previous behavior)
--   auto_resync — the supervisor repairs flagged tables with a forced
--                 snapshot on its next cadence
--   reject      — registration refuses sources whose included tables lack
--                 a usable key
ALTER TABLE databases ADD COLUMN keyless_policy TEXT NOT NULL DEFAULT 'quarantine';
PRAGMA user_version = 10;
