-- Which change-capture decoder a table's copy has streamed under since it
-- completed. A copy reads every value as text and stores it right; the
-- rows a stream applies afterwards are only as right as the binary that
-- decoded them. Generation 1 is the first binary that reads a negative
-- signed MEDIUMINT with its sign and keeps a BINARY(n) value's trailing
-- zero bytes. Set when a copy or resync completes. A table copied before
-- this column existed stays at 0: its streamed rows may have been decoded
-- by an earlier binary, and only a recopy settles that.
ALTER TABLE tables ADD COLUMN copy_generation INTEGER NOT NULL DEFAULT 0;
PRAGMA user_version = 24;
