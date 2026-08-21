//! Pintail's SQLite-backed control plane.
//!
//! This crate stores configuration and replication metadata only. Analytical
//! row data belongs exclusively to `pintail-store`.

use std::{collections::BTreeSet, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction};

mod backup;
mod control;

pub use backup::{
    BackupConfigRecord, BackupRecord, NewBackup, NewBackupConfig, RestoredCheckpoint,
    RestoredDatabase, RestoredTable,
};
pub use control::{
    ApiKeyRecord, AuditEventRecord, DatabaseRecord, DatabaseUpdate, DlqRecord, GoogleAdmission,
    InviteRecord, NewApiKey, NewAuditEvent, NewInvite, SyncRunRecord, TableRecord, UserRecord,
    WorkspaceMemberRecord, WorkspaceRecord,
};

const CURRENT_SCHEMA_VERSION: u32 = 17;

/// An initialized Pintail control-plane database.
pub struct MetaStore {
    connection: Connection,
}

/// A durable setting returned from an insert-if-absent operation.
#[derive(Debug, Eq, PartialEq)]
pub struct StoredSetting {
    value: String,
    inserted: bool,
}

/// Durable state of one source snapshot chunk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotChunkStatus {
    /// Planned but not started.
    Pending,
    /// Currently being read and published.
    Running,
    /// Segment publication and metadata checkpoint both completed.
    Completed,
    /// The most recent attempt failed.
    Error,
}

impl SnapshotChunkStatus {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "error" => Ok(Self::Error),
            other => bail!("unknown snapshot chunk status {other}"),
        }
    }
}

/// Persisted snapshot chunk progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotChunkRecord {
    /// Stable chunk identifier.
    pub chunk_id: String,
    /// Serialized exclusive lower key bound.
    pub lo_key_json: Option<String>,
    /// Serialized inclusive upper key bound.
    pub hi_key_json: Option<String>,
    /// Durable execution state.
    pub status: SnapshotChunkStatus,
    /// Rows published by the completed chunk.
    pub rows: u64,
}

/// Source position that owns the snapshot-to-stream handoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotCheckpointRecord {
    /// `gtid`, `filepos`, or `polling`.
    pub kind: String,
    /// Executed GTID set for GTID-capable sources.
    pub gtid_set: Option<String>,
    /// Binlog file captured with a GTID or file/position source.
    pub binlog_file: Option<String>,
    /// Binlog byte offset captured with a GTID or file/position source.
    pub binlog_pos: Option<u64>,
}

/// Durable polling position and cheap-probe token for one table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollStateRecord {
    /// Selected source cursor, absent for checksum-only tables.
    pub cursor_column: Option<String>,
    /// Serialized inclusive cursor boundary.
    pub cursor_json: Option<String>,
    /// Serialized row-count/MAX cheap-probe token.
    pub source_token_json: Option<String>,
    /// Source row count observed by the latest completed cycle.
    pub source_count: u64,
    /// Monotonic row version assigned to the latest completed poll cycle.
    pub version: u64,
    /// Last completed full primary-key reconciliation.
    pub last_reconcile_at: Option<String>,
}

/// Values committed after one polling WAL boundary.
pub struct PollStateUpdate<'a> {
    /// Selected source cursor, absent for checksum-only tables.
    pub cursor_column: Option<&'a str>,
    /// Serialized inclusive cursor boundary.
    pub cursor_json: Option<&'a str>,
    /// Serialized row-count/MAX cheap-probe token.
    pub source_token_json: Option<&'a str>,
    /// Source row count observed in this cycle.
    pub source_count: u64,
    /// Monotonic row version used by this cycle.
    pub version: u64,
    /// Whether this cycle completed a full delete reconciliation.
    pub reconciled: bool,
}

/// Persisted source/replica fingerprints for one polling chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollChunkStateRecord {
    /// Stable zero-based chunk identifier.
    pub chunk_id: String,
    /// Rows represented by the source aggregate.
    pub source_count: u64,
    /// Source-side aggregate checksum.
    pub source_checksum: String,
    /// Replica-side normalized checksum after the completed cycle.
    pub replica_checksum: String,
}

/// Values committed for one checksum chunk.
pub struct PollChunkStateUpdate<'a> {
    /// Stable zero-based chunk identifier.
    pub chunk_id: &'a str,
    /// Rows represented by the source aggregate.
    pub source_count: u64,
    /// Source-side aggregate checksum.
    pub source_checksum: &'a str,
    /// Replica-side normalized checksum after the completed cycle.
    pub replica_checksum: &'a str,
}

/// One persisted source-table schema generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaHistoryRecord {
    /// Monotonic table schema generation.
    pub version: u32,
    /// Source DDL that produced this generation, when available.
    pub ddl_text: Option<String>,
    /// Serialized probed source columns.
    pub columns_json: String,
    /// Time at which the generation was applied or quarantined.
    pub applied_at: String,
}

impl StoredSetting {
    /// Returns the durable value, whether newly inserted or previously stored.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns whether this call inserted the value.
    #[must_use]
    pub fn was_inserted(&self) -> bool {
        self.inserted
    }
}

impl MetaStore {
    /// Opens a control-plane database and applies all pending migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot open or configure the database, or
    /// when a migration cannot be applied atomically.
    pub fn open(path: &Path) -> Result<Self> {
        prepare_private_database_file(path)?;
        let mut connection = Connection::open(path)
            .with_context(|| format!("failed to open metadata database {}", path.display()))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("failed to configure SQLite busy timeout")?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .context("failed to enable SQLite foreign keys")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable SQLite WAL mode")?;

        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    /// Returns the schema version applied to this database.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot read the schema version pragma.
    pub fn schema_version(&self) -> Result<u32> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("failed to read metadata schema version")
    }

    /// Reads one setting value, when present.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings table cannot be queried.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .with_context(|| format!("failed to read metadata setting {key}"))
    }

    /// Writes or replaces one setting value.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings row cannot be written.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .with_context(|| format!("failed to write metadata setting {key}"))?;
        Ok(())
    }

    /// Deletes one setting when present.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings row cannot be removed.
    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM settings WHERE key = ?1", [key])
            .with_context(|| format!("failed to delete metadata setting {key}"))?;
        Ok(())
    }

    /// Inserts a setting when absent and returns its durable value.
    ///
    /// An existing setting always wins over the supplied candidate. This
    /// supports one-time secret generation without replacing keys on restart.
    ///
    /// # Errors
    ///
    /// Returns an error when `SQLite` cannot insert or read the setting.
    pub fn get_or_insert_setting(&self, key: &str, candidate: &str) -> Result<StoredSetting> {
        let inserted = self
            .connection
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO NOTHING",
                (key, candidate),
            )
            .with_context(|| format!("failed to initialize metadata setting {key}"))?
            == 1;
        let value = self
            .connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .with_context(|| format!("failed to read metadata setting {key}"))?;
        Ok(StoredSetting { value, inserted })
    }

    /// Registers or refreshes a source database for snapshot coordination.
    ///
    /// The encrypted DSN is treated as opaque control-plane data.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be written.
    pub fn upsert_database(
        &self,
        id: &str,
        name: &str,
        encrypted_dsn: &[u8],
        now: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO databases (\
                   id, name, mysql_dsn_encrypted, mode, effective_mode, state, \
                   created_at, updated_at\
                 ) VALUES (?1, ?2, ?3, 'auto', NULL, 'created', ?4, ?4) \
                 ON CONFLICT(id) DO UPDATE SET \
                   name = excluded.name, \
                   mysql_dsn_encrypted = excluded.mysql_dsn_encrypted, \
                   updated_at = excluded.updated_at",
                (id, name, encrypted_dsn, now),
            )
            .with_context(|| format!("failed to register source database {id}"))?;
        Ok(())
    }

    /// Assigns a database to a workspace. Set once, immediately after
    /// [`MetaStore::upsert_database`] creates the row via the HTTP API; a
    /// database never moves workspaces afterward.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be updated.
    pub fn set_database_workspace(&self, id: &str, workspace_id: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE databases SET workspace_id = ?2 WHERE id = ?1",
                (id, workspace_id),
            )
            .with_context(|| format!("failed to assign database {id} to a workspace"))?;
        Ok(())
    }

    /// Registers a table before its snapshot chunks are journaled.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent database is absent or the table row
    /// cannot be written.
    pub fn upsert_snapshot_table(
        &self,
        database_id: &str,
        table_name: &str,
        pk_json: Option<&str>,
        sort_key_json: Option<&str>,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO tables (\
                   db_id, name, state, pk_json, sort_key_json, schema_version\
                 ) VALUES (?1, ?2, 'snapshotting', ?3, ?4, 1) \
                 ON CONFLICT(db_id, name) DO UPDATE SET \
                   state = CASE \
                     WHEN tables.state IN ('streaming', 'polling') THEN tables.state \
                     ELSE 'snapshotting' \
                   END, \
                   pk_json = excluded.pk_json, \
                   sort_key_json = excluded.sort_key_json",
                (database_id, table_name, pk_json, sort_key_json),
            )
            .with_context(|| {
                format!("failed to register snapshot table {database_id}.{table_name}")
            })?;
        Ok(())
    }

    /// Persists the source position captured before snapshot transactions
    /// begin.
    ///
    /// Exactly one of `gtid_set` or `binlog_file`/`binlog_pos` should be
    /// populated according to `kind`.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint cannot be written.
    pub fn upsert_snapshot_checkpoint(
        &self,
        database_id: &str,
        kind: &str,
        gtid_set: Option<&str>,
        binlog_file: Option<&str>,
        binlog_pos: Option<u64>,
        now: &str,
    ) -> Result<()> {
        if !matches!(kind, "gtid" | "filepos") {
            bail!("snapshot checkpoint kind must be gtid or filepos");
        }
        let binlog_pos = binlog_pos
            .map(i64::try_from)
            .transpose()
            .context("binlog position exceeds i64")?;
        self.connection
            .execute(
                "INSERT INTO checkpoints (\
                   db_id, kind, gtid_set, binlog_file, binlog_pos, updated_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(db_id) DO UPDATE SET \
                   kind = excluded.kind, gtid_set = excluded.gtid_set, \
                   binlog_file = excluded.binlog_file, \
                   binlog_pos = excluded.binlog_pos, \
                   updated_at = excluded.updated_at",
                (database_id, kind, gtid_set, binlog_file, binlog_pos, now),
            )
            .with_context(|| format!("failed to persist snapshot checkpoint for {database_id}"))?;
        Ok(())
    }

    /// Persists the first snapshot handoff position and leaves it unchanged on
    /// resume.
    ///
    /// A new full snapshot must explicitly replace or delete the checkpoint;
    /// a resumed snapshot must replay CDC from the original position so
    /// changes made while Pintail was stopped are not lost.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint is invalid or cannot be written.
    pub fn insert_snapshot_checkpoint_if_absent(
        &self,
        database_id: &str,
        kind: &str,
        gtid_set: Option<&str>,
        binlog_file: Option<&str>,
        binlog_pos: Option<u64>,
        now: &str,
    ) -> Result<()> {
        if !matches!(kind, "gtid" | "filepos" | "polling") {
            bail!("snapshot checkpoint kind must be gtid, filepos, or polling");
        }
        let binlog_pos = binlog_pos
            .map(i64::try_from)
            .transpose()
            .context("binlog position exceeds i64")?;
        self.connection
            .execute(
                "INSERT INTO checkpoints (\
                   db_id, kind, gtid_set, binlog_file, binlog_pos, updated_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(db_id) DO NOTHING",
                (database_id, kind, gtid_set, binlog_file, binlog_pos, now),
            )
            .with_context(|| {
                format!("failed to initialize snapshot checkpoint for {database_id}")
            })?;
        Ok(())
    }

    /// Returns the snapshot-to-stream handoff position, when initialized.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint cannot be read or decoded.
    pub fn snapshot_checkpoint(
        &self,
        database_id: &str,
    ) -> Result<Option<SnapshotCheckpointRecord>> {
        let raw = self
            .connection
            .query_row(
                "SELECT kind, gtid_set, binlog_file, binlog_pos \
                 FROM checkpoints WHERE db_id = ?1",
                [database_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .context("failed to read snapshot checkpoint")?;
        raw.map(|(kind, gtid_set, binlog_file, binlog_pos)| {
            let binlog_pos = binlog_pos
                .map(u64::try_from)
                .transpose()
                .context("snapshot checkpoint contains a negative binlog position")?;
            Ok(SnapshotCheckpointRecord {
                kind,
                gtid_set,
                binlog_file,
                binlog_pos,
            })
        })
        .transpose()
    }

    /// Returns one table's durable polling state.
    ///
    /// # Errors
    ///
    /// Returns an error when the row cannot be queried or contains negative
    /// counters.
    pub fn poll_state(
        &self,
        database_id: &str,
        table_name: &str,
    ) -> Result<Option<PollStateRecord>> {
        type RawPollState = (
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
            Option<String>,
        );
        let raw: Option<RawPollState> = self
            .connection
            .query_row(
                "SELECT cursor_column, cursor_json, source_token_json, \
                        source_count, version, last_reconcile_at \
                 FROM poll_states WHERE db_id = ?1 AND table_name = ?2",
                (database_id, table_name),
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .context("failed to read polling state")?;
        raw.map(
            |(
                cursor_column,
                cursor_json,
                source_token_json,
                source_count,
                version,
                last_reconcile_at,
            )| {
                Ok(PollStateRecord {
                    cursor_column,
                    cursor_json,
                    source_token_json,
                    source_count: u64::try_from(source_count)
                        .context("poll source count is negative")?,
                    version: u64::try_from(version).context("poll version is negative")?,
                    last_reconcile_at,
                })
            },
        )
        .transpose()
    }

    /// Returns durable checksum fingerprints for one table in chunk order.
    ///
    /// # Errors
    ///
    /// Returns an error when rows cannot be queried or contain negative
    /// counts.
    pub fn poll_chunk_states(
        &self,
        database_id: &str,
        table_name: &str,
    ) -> Result<Vec<PollChunkStateRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT chunk_id, source_count, source_checksum, replica_checksum \
                 FROM poll_chunk_states WHERE db_id = ?1 AND table_name = ?2 \
                 ORDER BY CAST(chunk_id AS INTEGER), chunk_id",
            )
            .context("failed to prepare polling chunk-state query")?;
        statement
            .query_map((database_id, table_name), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("failed to query polling chunk states")?
            .map(|row| {
                let (chunk_id, source_count, source_checksum, replica_checksum) =
                    row.context("failed to decode polling chunk state")?;
                Ok(PollChunkStateRecord {
                    chunk_id,
                    source_count: u64::try_from(source_count)
                        .context("poll chunk source count is negative")?,
                    source_checksum,
                    replica_checksum,
                })
            })
            .collect()
    }

    /// Commits one polling position after its table WAL is synchronized.
    ///
    /// # Errors
    ///
    /// Returns an error when counters exceed `SQLite`'s range or the
    /// control-plane transaction cannot commit.
    pub fn commit_poll_state(
        &mut self,
        database_id: &str,
        table_name: &str,
        update: &PollStateUpdate<'_>,
        now: &str,
    ) -> Result<()> {
        self.commit_poll_state_inner(database_id, table_name, update, None, now)
    }

    /// Atomically replaces checksum fingerprints and commits a polling
    /// position after the table WAL is synchronized.
    ///
    /// # Errors
    ///
    /// Returns an error when counters exceed `SQLite`'s range or the
    /// control-plane transaction cannot commit.
    pub fn commit_poll_state_with_chunks(
        &mut self,
        database_id: &str,
        table_name: &str,
        update: &PollStateUpdate<'_>,
        chunks: &[PollChunkStateUpdate<'_>],
        now: &str,
    ) -> Result<()> {
        self.commit_poll_state_inner(database_id, table_name, update, Some(chunks), now)
    }

    /// Records a CDC-side key reconciliation without changing the database's
    /// replication mode or binlog checkpoint.
    ///
    /// The caller synchronizes the table WAL before invoking this method.
    ///
    /// # Errors
    ///
    /// Returns an error when counters exceed `SQLite`'s range or the
    /// transaction cannot commit.
    pub fn commit_cdc_reconciliation(
        &mut self,
        database_id: &str,
        table_name: &str,
        source_count: u64,
        version: u64,
        now: &str,
    ) -> Result<()> {
        let source_count =
            i64::try_from(source_count).context("CDC reconciliation source count exceeds i64")?;
        let version = i64::try_from(version).context("CDC reconciliation version exceeds i64")?;
        let transaction = self
            .connection
            .transaction()
            .context("failed to begin CDC reconciliation checkpoint")?;
        transaction
            .execute(
                "INSERT INTO poll_states (\
                   db_id, table_name, source_count, version, last_reconcile_at, updated_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
                 ON CONFLICT(db_id, table_name) DO UPDATE SET \
                   source_count = excluded.source_count, \
                   version = MAX(poll_states.version, excluded.version), \
                   last_reconcile_at = excluded.last_reconcile_at, \
                   updated_at = excluded.updated_at",
                (database_id, table_name, source_count, version, now),
            )
            .context("failed to persist CDC reconciliation state")?;
        transaction
            .execute(
                "UPDATE tables SET last_reconcile_at = ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, now),
            )
            .context("failed to update CDC table reconciliation time")?;
        transaction
            .commit()
            .context("failed to commit CDC reconciliation checkpoint")
    }

    fn commit_poll_state_inner(
        &mut self,
        database_id: &str,
        table_name: &str,
        update: &PollStateUpdate<'_>,
        chunks: Option<&[PollChunkStateUpdate<'_>]>,
        now: &str,
    ) -> Result<()> {
        let source_count =
            i64::try_from(update.source_count).context("poll source count exceeds i64")?;
        let version = i64::try_from(update.version).context("poll version exceeds i64")?;
        let reconcile_at = update.reconciled.then_some(now);
        let transaction = self
            .connection
            .transaction()
            .context("failed to begin polling checkpoint")?;
        if let Some(chunks) = chunks {
            transaction
                .execute(
                    "DELETE FROM poll_chunk_states WHERE db_id = ?1 AND table_name = ?2",
                    (database_id, table_name),
                )
                .context("failed to clear stale polling chunk states")?;
            for chunk in chunks {
                let chunk_count = i64::try_from(chunk.source_count)
                    .context("poll chunk source count exceeds i64")?;
                transaction
                    .execute(
                        "INSERT INTO poll_chunk_states (\
                           db_id, table_name, chunk_id, source_count, source_checksum, \
                           replica_checksum, updated_at\
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        (
                            database_id,
                            table_name,
                            chunk.chunk_id,
                            chunk_count,
                            chunk.source_checksum,
                            chunk.replica_checksum,
                            now,
                        ),
                    )
                    .context("failed to persist polling chunk state")?;
            }
        }
        transaction
            .execute(
                "INSERT INTO poll_states (\
                   db_id, table_name, cursor_column, cursor_json, source_token_json, \
                   source_count, version, last_reconcile_at, updated_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(db_id, table_name) DO UPDATE SET \
                   cursor_column = excluded.cursor_column, \
                   cursor_json = excluded.cursor_json, \
                   source_token_json = excluded.source_token_json, \
                   source_count = excluded.source_count, \
                   version = excluded.version, \
                   last_reconcile_at = COALESCE(\
                     excluded.last_reconcile_at, poll_states.last_reconcile_at\
                   ), \
                   updated_at = excluded.updated_at",
                (
                    database_id,
                    table_name,
                    update.cursor_column,
                    update.cursor_json,
                    update.source_token_json,
                    source_count,
                    version,
                    reconcile_at,
                    now,
                ),
            )
            .context("failed to persist polling state")?;
        transaction
            .execute(
                "INSERT INTO checkpoints (db_id, kind, poll_cursors_json, updated_at) \
                 VALUES (?1, 'polling', '{}', ?2) \
                 ON CONFLICT(db_id) DO UPDATE SET \
                   kind = 'polling', gtid_set = NULL, binlog_file = NULL, \
                   binlog_pos = NULL, poll_cursors_json = '{}', \
                   updated_at = excluded.updated_at",
                (database_id, now),
            )
            .context("failed to persist polling database checkpoint")?;
        transaction
            .execute(
                "UPDATE tables SET state = 'polling', last_error = NULL, \
                   last_reconcile_at = COALESCE(?3, last_reconcile_at) \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, reconcile_at),
            )
            .context("failed to mark table polling")?;
        // A checkpoint commit is not entitled to overwrite a mode switch
        // that landed while the cycle ran: an in-flight poll pass finishing
        // after an operator's polling-to-cdc switch wrote 'polling' back
        // and every later cycle read it and re-wrote it - the database
        // polled forever under mode cdc. The supervisor's completion write
        // carries this guard already; the checkpoint path is the one that
        // actually fired (three fast-cadence e2e runs in four).
        transaction
            .execute(
                "UPDATE databases SET state = 'polling', effective_mode = 'polling', \
                   updated_at = ?2 WHERE id = ?1 AND mode IN ('polling', 'auto')",
                (database_id, now),
            )
            .context("failed to mark database polling")?;
        transaction
            .commit()
            .context("failed to commit polling checkpoint")
    }

    /// Commits a CDC source checkpoint after every touched WAL has been
    /// synchronized.
    ///
    /// # Errors
    ///
    /// Returns an error when the position is invalid or the control-plane
    /// transaction cannot commit.
    pub fn commit_cdc_checkpoint(
        &mut self,
        database_id: &str,
        checkpoint: &SnapshotCheckpointRecord,
        touched_tables: &[String],
        now: &str,
    ) -> Result<()> {
        if !matches!(checkpoint.kind.as_str(), "gtid" | "filepos") {
            bail!("CDC checkpoint kind must be gtid or filepos");
        }
        let binlog_pos = checkpoint
            .binlog_pos
            .map(i64::try_from)
            .transpose()
            .context("binlog position exceeds i64")?;
        let transaction = self
            .connection
            .transaction()
            .context("failed to begin CDC checkpoint")?;
        transaction
            .execute(
                "INSERT INTO checkpoints (\
                   db_id, kind, gtid_set, binlog_file, binlog_pos, updated_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(db_id) DO UPDATE SET \
                   kind = excluded.kind, gtid_set = excluded.gtid_set, \
                   binlog_file = excluded.binlog_file, \
                   binlog_pos = excluded.binlog_pos, \
                   updated_at = excluded.updated_at",
                (
                    database_id,
                    &checkpoint.kind,
                    checkpoint.gtid_set.as_deref(),
                    checkpoint.binlog_file.as_deref(),
                    binlog_pos,
                    now,
                ),
            )
            .context("failed to persist CDC checkpoint")?;
        for table_name in touched_tables {
            transaction
                .execute(
                    "UPDATE tables SET state = 'streaming', last_error = NULL \
                     WHERE db_id = ?1 AND name = ?2 AND state != 'needs_resync'",
                    (database_id, table_name),
                )
                .with_context(|| format!("failed to mark {database_id}.{table_name} streaming"))?;
        }
        // Mirror of the polling checkpoint's guard: a CDC checkpoint that
        // lands after a cdc-to-polling switch must not flip the mode back.
        transaction
            .execute(
                "UPDATE databases SET state = 'streaming', effective_mode = 'cdc', \
                   updated_at = ?2 WHERE id = ?1 AND mode IN ('cdc', 'auto')",
                (database_id, now),
            )
            .context("failed to mark database streaming")?;
        transaction
            .commit()
            .context("failed to commit CDC checkpoint")
    }

    /// Marks one table as requiring a new snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be persisted.
    pub fn mark_table_needs_resync(
        &self,
        database_id: &str,
        table_name: &str,
        error: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE tables SET state = 'needs_resync', last_error = ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, error),
            )
            .with_context(|| format!("failed to mark {database_id}.{table_name} for resnapshot"))?;
        Ok(())
    }

    /// Puts one table into `snapshotting`, leaving the rest of the database
    /// alone.
    ///
    /// Deliberately narrower than [`MetaStore::reset_for_resnapshot`]: that one
    /// clears the database's chunk journal and source checkpoint, which every
    /// other table's stream is reading. A single-table resnapshot keeps both,
    /// and is made safe against replaying its own history by the per-table
    /// snapshot fence the caller records instead.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be persisted.
    pub fn begin_table_resnapshot(&self, database_id: &str, table_name: &str) -> Result<()> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin table resnapshot")?;
        // The chunk journal is how a snapshot resumes: chunks recorded
        // completed are skipped. Emptying the store without clearing it makes
        // the copy a no-op that still reports the previous run's totals - a
        // resnapshot that says it moved 200 rows and leaves the table empty.
        transaction
            .execute(
                "DELETE FROM snapshot_chunks WHERE db_id = ?1 AND table_name = ?2",
                (database_id, table_name),
            )
            .with_context(|| {
                format!("failed to clear the chunk journal for {database_id}.{table_name}")
            })?;
        // NOCASE, and loud when nothing matched: the API validates the table
        // name case-insensitively, so a name that differs only in case
        // reached here, updated zero rows, and left the table flagged
        // needs_resync while the snapshot itself ran and reported success.
        let changed = transaction
            .execute(
                "UPDATE tables SET state = 'snapshotting', rows_synced = 0, \
                   last_error = NULL WHERE db_id = ?1 AND name = ?2 COLLATE NOCASE",
                (database_id, table_name),
            )
            .with_context(|| {
                format!("failed to start a resnapshot of {database_id}.{table_name}")
            })?;
        if changed == 0 {
            bail!("{database_id}.{table_name} is not a tracked table");
        }
        transaction
            .commit()
            .context("failed to commit table resnapshot start")
    }

    /// Returns one table to a replicating state once its snapshot is durable.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be persisted.
    pub fn finish_table_resnapshot(
        &self,
        database_id: &str,
        table_name: &str,
        state: &str,
    ) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE tables SET state = ?3, last_error = NULL \
                 WHERE db_id = ?1 AND name = ?2 COLLATE NOCASE",
                (database_id, table_name, state),
            )
            .with_context(|| {
                format!("failed to finish the resnapshot of {database_id}.{table_name}")
            })?;
        if changed == 0 {
            bail!("{database_id}.{table_name} is not a tracked table");
        }
        Ok(())
    }

    /// Returns included tables whose CDC stream must wait for a new snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be queried.
    pub fn tables_needing_resync(&self, database_id: &str) -> Result<BTreeSet<String>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT name FROM tables \
                 WHERE db_id = ?1 AND state = 'needs_resync' ORDER BY name",
            )
            .context("failed to prepare resnapshot table query")?;
        statement
            .query_map([database_id], |row| row.get(0))
            .context("failed to query resnapshot tables")?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .context("failed to decode resnapshot tables")
    }

    /// Persists a table schema generation and advances the table catalog
    /// version in one transaction.
    ///
    /// Replaying the same generation is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent table is absent or the transaction
    /// cannot commit.
    pub fn record_schema_history(
        &mut self,
        database_id: &str,
        table_name: &str,
        version: u32,
        ddl_text: Option<&str>,
        columns_json: &str,
        applied_at: &str,
    ) -> Result<()> {
        let version = i64::from(version);
        let transaction = self
            .connection
            .transaction()
            .context("failed to begin schema-history update")?;
        transaction
            .execute(
                "INSERT INTO schema_history (\
                   db_id, table_name, version, ddl_text, columns_json, applied_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(db_id, table_name, version) DO UPDATE SET \
                   ddl_text = excluded.ddl_text, \
                   columns_json = excluded.columns_json, \
                   applied_at = excluded.applied_at",
                (
                    database_id,
                    table_name,
                    version,
                    ddl_text,
                    columns_json,
                    applied_at,
                ),
            )
            .context("failed to persist schema history")?;
        transaction
            .execute(
                "UPDATE tables SET schema_version = MAX(schema_version, ?3) \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, version),
            )
            .context("failed to advance table schema version")?;
        transaction
            .commit()
            .context("failed to commit schema-history update")
    }

    /// Returns a table's persisted schema generations in version order.
    ///
    /// # Errors
    ///
    /// Returns an error when history cannot be queried or contains an invalid
    /// version.
    pub fn schema_history(
        &self,
        database_id: &str,
        table_name: &str,
    ) -> Result<Vec<SchemaHistoryRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT version, ddl_text, columns_json, applied_at \
                 FROM schema_history WHERE db_id = ?1 AND table_name = ?2 \
                 ORDER BY version",
            )
            .context("failed to prepare schema-history query")?;
        statement
            .query_map((database_id, table_name), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("failed to query schema history")?
            .map(|row| {
                let (version, ddl_text, columns_json, applied_at) =
                    row.context("failed to decode schema history")?;
                Ok(SchemaHistoryRecord {
                    version: u32::try_from(version)
                        .context("schema history contains an invalid version")?,
                    ddl_text,
                    columns_json,
                    applied_at,
                })
            })
            .collect()
    }

    /// Marks a dropped source table as retained, read-only orphaned data.
    ///
    /// # Errors
    ///
    /// Returns an error when the table state cannot be persisted.
    pub fn mark_table_orphaned(
        &self,
        database_id: &str,
        table_name: &str,
        ddl_text: &str,
        now: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE tables SET state = 'excluded', orphaned_at = ?4, \
                   last_error = ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, ddl_text, now),
            )
            .with_context(|| format!("failed to mark {database_id}.{table_name} orphaned"))?;
        Ok(())
    }

    /// Clears prior snapshot progress and prepares a fresh source handoff.
    ///
    /// The caller resets table storage before invoking the snapshot engine.
    ///
    /// # Errors
    ///
    /// Returns an error when the control-plane transaction cannot commit.
    pub fn begin_resnapshot(&self, database_id: &str, now: &str) -> Result<()> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin resnapshot reset")?;
        transaction
            .execute(
                "DELETE FROM snapshot_chunks WHERE db_id = ?1",
                [database_id],
            )
            .context("failed to clear snapshot chunk journal")?;
        transaction
            .execute("DELETE FROM checkpoints WHERE db_id = ?1", [database_id])
            .context("failed to clear source checkpoint")?;
        transaction
            .execute(
                "UPDATE tables SET state = 'snapshotting', rows_synced = 0, \
                   last_error = NULL WHERE db_id = ?1 AND state != 'excluded'",
                [database_id],
            )
            .context("failed to reset table snapshot state")?;
        transaction
            .execute(
                "UPDATE databases SET state = 'snapshotting', updated_at = ?2 \
                 WHERE id = ?1",
                (database_id, now),
            )
            .context("failed to reset database snapshot state")?;
        transaction
            .commit()
            .context("failed to commit resnapshot reset")
    }

    /// Marks every included table as requiring a new snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be persisted.
    pub fn mark_database_needs_resync(&self, database_id: &str, error: &str) -> Result<()> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin resnapshot state update")?;
        transaction
            .execute(
                "UPDATE tables SET state = 'needs_resync', last_error = ?2 \
                 WHERE db_id = ?1 AND state != 'excluded'",
                (database_id, error),
            )
            .context("failed to mark tables for resnapshot")?;
        transaction
            .execute(
                "UPDATE databases SET state = 'needs_resync' WHERE id = ?1",
                [database_id],
            )
            .context("failed to mark database for resnapshot")?;
        transaction
            .commit()
            .context("failed to commit resnapshot state")
    }

    /// Adds a failed source event to the durable dead-letter queue.
    ///
    /// Replaying the same binlog position is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error when the DLQ record cannot be written.
    pub fn record_dlq(
        &self,
        id: &str,
        database_id: &str,
        table_name: Option<&str>,
        event_json: &str,
        error: &str,
        now: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO dlq (id, db_id, table_name, event_json, error, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(id) DO UPDATE SET error = excluded.error",
                (id, database_id, table_name, event_json, error, now),
            )
            .context("failed to record CDC dead-letter event")?;
        Ok(())
    }

    /// Returns completed chunk identifiers for one table.
    ///
    /// # Errors
    ///
    /// Returns an error when progress cannot be read.
    pub fn completed_snapshot_chunks(
        &self,
        database_id: &str,
        table_name: &str,
    ) -> Result<BTreeSet<String>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT chunk_id FROM snapshot_chunks \
                 WHERE db_id = ?1 AND table_name = ?2 AND status = 'completed' \
                 ORDER BY chunk_id",
            )
            .context("failed to prepare completed snapshot chunk query")?;
        let chunks = statement
            .query_map((database_id, table_name), |row| row.get(0))
            .context("failed to query completed snapshot chunks")?
            .collect::<rusqlite::Result<BTreeSet<String>>>()
            .context("failed to decode completed snapshot chunks")?;
        Ok(chunks)
    }

    /// Marks a chunk running, resetting a prior failed/interrupted attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the chunk checkpoint cannot be persisted.
    pub fn start_snapshot_chunk(
        &self,
        database_id: &str,
        table_name: &str,
        chunk_id: &str,
        lo_key_json: Option<&str>,
        hi_key_json: Option<&str>,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO snapshot_chunks (\
                   db_id, table_name, chunk_id, lo_key_json, hi_key_json, status, rows\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', 0) \
                 ON CONFLICT(db_id, table_name, chunk_id) DO UPDATE SET \
                   lo_key_json = excluded.lo_key_json, \
                   hi_key_json = excluded.hi_key_json, \
                   status = 'running', rows = 0 \
                 WHERE snapshot_chunks.status != 'completed'",
                (database_id, table_name, chunk_id, lo_key_json, hi_key_json),
            )
            .with_context(|| {
                format!("failed to start snapshot chunk {database_id}.{table_name}/{chunk_id}")
            })?;
        Ok(())
    }

    /// Marks a durably published chunk completed and advances table progress.
    ///
    /// # Errors
    ///
    /// Returns an error if the two control-plane updates cannot commit
    /// atomically.
    pub fn complete_snapshot_chunk(
        &mut self,
        database_id: &str,
        table_name: &str,
        chunk_id: &str,
        rows: u64,
    ) -> Result<()> {
        let rows_i64 = i64::try_from(rows).context("snapshot chunk row count exceeds i64")?;
        let transaction = self
            .connection
            .transaction()
            .context("failed to begin snapshot chunk completion")?;
        let changed = transaction
            .execute(
                "UPDATE snapshot_chunks SET status = 'completed', rows = ?4 \
                 WHERE db_id = ?1 AND table_name = ?2 AND chunk_id = ?3 \
                   AND status != 'completed'",
                (database_id, table_name, chunk_id, rows_i64),
            )
            .context("failed to complete snapshot chunk")?;
        if changed == 0 {
            let status = transaction
                .query_row(
                    "SELECT status FROM snapshot_chunks \
                     WHERE db_id = ?1 AND table_name = ?2 AND chunk_id = ?3",
                    (database_id, table_name, chunk_id),
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .context("failed to inspect snapshot chunk state")?;
            if status.as_deref() == Some("completed") {
                return transaction
                    .commit()
                    .context("failed to commit idempotent snapshot chunk completion");
            }
            bail!("snapshot chunk {database_id}.{table_name}/{chunk_id} was not started");
        }
        transaction
            .execute(
                "UPDATE tables SET rows_synced = rows_synced + ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, rows_i64),
            )
            .context("failed to advance snapshot table progress")?;
        transaction
            .commit()
            .context("failed to commit snapshot chunk completion")
    }

    /// Marks the table snapshot complete.
    ///
    /// # Errors
    ///
    /// Returns an error if the table state cannot be updated.
    pub fn complete_snapshot_table(&self, database_id: &str, table_name: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE tables SET state = 'pending', last_error = NULL \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name),
            )
            .with_context(|| {
                format!("failed to complete snapshot table {database_id}.{table_name}")
            })?;
        Ok(())
    }

    /// Records a table-level snapshot error.
    ///
    /// # Errors
    ///
    /// Returns an error if the table state cannot be updated.
    pub fn fail_snapshot_table(
        &self,
        database_id: &str,
        table_name: &str,
        error: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "UPDATE tables SET state = 'error', last_error = ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, error),
            )
            .with_context(|| format!("failed to record snapshot error for {table_name}"))?;
        Ok(())
    }

    /// Reads every chunk record for verification and progress surfaces.
    ///
    /// # Errors
    ///
    /// Returns an error when records cannot be queried or decoded.
    pub fn snapshot_chunks(
        &self,
        database_id: &str,
        table_name: &str,
    ) -> Result<Vec<SnapshotChunkRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT chunk_id, lo_key_json, hi_key_json, status, rows \
                 FROM snapshot_chunks WHERE db_id = ?1 AND table_name = ?2 \
                 ORDER BY chunk_id",
            )
            .context("failed to prepare snapshot chunk query")?;
        let rows = statement
            .query_map((database_id, table_name), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            })
            .context("failed to query snapshot chunks")?;
        let mut chunks = Vec::new();
        for row in rows {
            let (chunk_id, lo_key_json, hi_key_json, status, rows) =
                row.context("failed to decode snapshot chunk")?;
            chunks.push(SnapshotChunkRecord {
                chunk_id,
                lo_key_json,
                hi_key_json,
                status: SnapshotChunkStatus::parse(&status)?,
                rows,
            });
        }
        Ok(chunks)
    }
}

#[cfg(unix)]
fn prepare_private_database_file(path: &Path) -> Result<()> {
    use std::{
        fs::{OpenOptions, Permissions},
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| {
            format!(
                "failed to create private metadata database {}",
                path.display()
            )
        })?;
    file.set_permissions(Permissions::from_mode(0o600))
        .with_context(|| {
            format!(
                "failed to secure metadata database permissions {}",
                path.display()
            )
        })
}

#[cfg(not(unix))]
fn prepare_private_database_file(_path: &Path) -> Result<()> {
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let found: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .context("failed to read metadata schema version")?;
    if found > CURRENT_SCHEMA_VERSION {
        bail!(
            "metadata schema version {found} is newer than this binary supports ({CURRENT_SCHEMA_VERSION})"
        );
    }

    if found == 0 {
        migration_v1(connection.transaction()?)?;
    }
    if found < 2 {
        migration_v2(connection.transaction()?)?;
    }
    if found < 3 {
        migration_v3(connection.transaction()?)?;
    }
    if found < 4 {
        migration_v4(connection.transaction()?)?;
    }
    if found < 5 {
        migration_v5(connection.transaction()?)?;
    }
    if found < 6 {
        migration_v6(connection.transaction()?)?;
    }
    if found < 7 {
        migration_v7(connection.transaction()?)?;
    }
    if found < 8 {
        migration_v8(connection)?;
    }
    if found < 9 {
        migration_v9(connection.transaction()?)?;
    }
    if found < 10 {
        migration_v10(connection.transaction()?)?;
    }
    if found < 11 {
        migration_v11(connection.transaction()?)?;
    }
    if found < 12 {
        migration_v12(connection.transaction()?)?;
    }
    if found < 13 {
        migration_v13(connection.transaction()?)?;
    }
    if found < 14 {
        migration_v14(connection.transaction()?)?;
    }
    if found < 15 {
        migration_v15(connection.transaction()?)?;
    }
    if found < 16 {
        migration_v16(connection.transaction()?)?;
    }
    if found < 17 {
        migration_v17(connection.transaction()?)?;
    }
    Ok(())
}

fn migration_v1(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .context("failed to apply metadata migration 1")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 1")
}

fn migration_v2(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/002_polling.sql"))
        .context("failed to apply metadata migration 2")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 2")
}

fn migration_v3(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/003_poll_checksums.sql"))
        .context("failed to apply metadata migration 3")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 3")
}

fn migration_v4(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/004_schema_tracking.sql"))
        .context("failed to apply metadata migration 4")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 4")
}

fn migration_v5(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/005_api_control.sql"))
        .context("failed to apply metadata migration 5")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 5")
}

fn migration_v6(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/006_wire_auth.sql"))
        .context("failed to apply metadata migration 6")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 6")
}

fn migration_v7(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/007_backups.sql"))
        .context("failed to apply metadata migration 7")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 7")
}

fn migration_v8(connection: &mut Connection) -> Result<()> {
    connection
        .pragma_update(None, "foreign_keys", false)
        .context("failed to disable foreign keys for metadata migration 8")?;
    let migration = (|| {
        let transaction = connection.transaction()?;
        transaction
            .execute_batch(include_str!("../migrations/008_restored_tables.sql"))
            .context("failed to apply metadata migration 8")?;
        transaction
            .commit()
            .context("failed to commit metadata migration 8")
    })();
    let reenabled = connection
        .pragma_update(None, "foreign_keys", true)
        .context("failed to re-enable foreign keys after metadata migration 8");
    migration?;
    reenabled?;
    let violations: u64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .context("failed to verify metadata foreign keys after migration 8")?;
    if violations != 0 {
        bail!("metadata migration 8 left {violations} foreign key violations");
    }
    Ok(())
}

fn migration_v9(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/009_backup_retention.sql"))
        .context("failed to apply metadata migration 9")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 9")
}

fn migration_v10(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/010_keyless_policy.sql"))
        .context("failed to apply metadata migration 10")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 10")
}

fn migration_v11(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/011_backup_verification.sql"))
        .context("failed to apply metadata migration 11")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 11")
}

fn migration_v12(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/012_backup_full_cadence.sql"))
        .context("failed to apply metadata migration 12")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 12")
}

fn migration_v13(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/013_caching_sha2.sql"))
        .context("failed to apply metadata migration 13")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 13")
}

fn migration_v14(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/014_database_kind.sql"))
        .context("failed to apply metadata migration 14")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 14")
}

fn migration_v15(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/015_workspaces.sql"))
        .context("failed to apply metadata migration 15")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 15")
}

fn migration_v16(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/016_oauth_invites_audit.sql"))
        .context("failed to apply metadata migration 16")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 16")
}

fn migration_v17(transaction: Transaction<'_>) -> Result<()> {
    transaction
        .execute_batch(include_str!("../migrations/017_audit_client_ip.sql"))
        .context("failed to apply metadata migration 17")?;
    transaction
        .commit()
        .context("failed to commit metadata migration 17")
}
