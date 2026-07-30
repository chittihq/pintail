//! Pintail's SQLite-backed control plane.
//!
//! This crate stores configuration and replication metadata only. Analytical
//! row data belongs exclusively to `pintail-store`.

use std::{collections::BTreeSet, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction};

const CURRENT_SCHEMA_VERSION: u32 = 1;

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
                     WHERE db_id = ?1 AND name = ?2",
                    (database_id, table_name),
                )
                .with_context(|| format!("failed to mark {database_id}.{table_name} streaming"))?;
        }
        transaction
            .execute(
                "UPDATE databases SET state = 'streaming', effective_mode = 'cdc', \
                   updated_at = ?2 WHERE id = ?1",
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
