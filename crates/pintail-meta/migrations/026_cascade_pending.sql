-- Parent keys a source-side foreign-key action may have followed without a
-- row event: the old key of every deleted parent row, and both keys of a
-- parent row whose key moved, queued per flagged child table. Change capture
-- writes them in the transaction that moves its checkpoint past the parent
-- events; the cascade pass reads them, repairs the children that reference
-- them, and deletes the ones it consumed. A NULL key marks a queue that
-- outgrew its cap: that child is compared in full on its next pass.
CREATE TABLE IF NOT EXISTS cascade_pending (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    db_id      TEXT NOT NULL REFERENCES databases(id) ON DELETE CASCADE,
    child      TEXT NOT NULL,
    parent     TEXT NOT NULL,
    key_json   TEXT
);
CREATE INDEX IF NOT EXISTS idx_cascade_pending_child ON cascade_pending(db_id, child, seq);

PRAGMA user_version = 26;
