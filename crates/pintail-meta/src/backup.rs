use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension as _, params};

use crate::MetaStore;

/// Encrypted S3-compatible destination for one mirrored database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupConfigRecord {
    pub database_id: String,
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub encrypted_access_key_id: Option<Vec<u8>>,
    pub encrypted_secret_access_key: Option<Vec<u8>>,
    pub schedule_minutes: u64,
    pub enabled: bool,
    pub retain_count: u64,
    /// Restore each completed backup into a scratch directory and record
    /// the checksum-verified outcome.
    pub verify_restore: bool,
    /// Force a full backup every Nth scheduled run; zero chains
    /// incrementals after the first full indefinitely.
    pub full_every: u64,
    pub updated_at: String,
}

/// Values used to create or replace a backup destination.
pub struct NewBackupConfig<'a> {
    pub database_id: &'a str,
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub endpoint: Option<&'a str>,
    pub region: &'a str,
    pub encrypted_access_key_id: Option<&'a [u8]>,
    pub encrypted_secret_access_key: Option<&'a [u8]>,
    pub schedule_minutes: u64,
    pub enabled: bool,
    pub retain_count: u64,
    pub verify_restore: bool,
    pub full_every: u64,
    pub now: &'a str,
}

/// Durable audit record for one full or incremental backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupRecord {
    pub id: String,
    pub database_id: String,
    pub kind: String,
    pub parent_id: Option<String>,
    pub object_prefix: String,
    pub status: String,
    pub bytes: u64,
    pub object_count: u64,
    pub error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    /// When a post-backup restore validation succeeded.
    pub verified_at: Option<String>,
    /// Why the last restore validation failed, when it did.
    pub verify_error: Option<String>,
}

/// Values required to start a durable backup run.
pub struct NewBackup<'a> {
    pub id: &'a str,
    pub database_id: &'a str,
    pub kind: &'a str,
    pub parent_id: Option<&'a str>,
    pub object_prefix: &'a str,
    pub started_at: &'a str,
}

/// Control-plane values restored alongside verified table objects.
pub struct RestoredDatabase<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub probe_json: &'a str,
    pub effective_mode: &'a str,
    pub tables: &'a [RestoredTable<'a>],
    pub checkpoint: Option<RestoredCheckpoint<'a>>,
    pub now: &'a str,
}

/// One restored table control-plane row.
pub struct RestoredTable<'a> {
    pub name: &'a str,
    pub primary_key_json: Option<&'a str>,
    pub cursor_column: Option<&'a str>,
    pub sort_key_json: Option<&'a str>,
    pub rows_synced: u64,
    pub schema_version: u32,
    pub soft_delete_column: Option<&'a str>,
}

/// One restored source checkpoint retained for audit and optional recovery.
pub struct RestoredCheckpoint<'a> {
    pub kind: &'a str,
    pub gtid_set: Option<&'a str>,
    pub binlog_file: Option<&'a str>,
    pub binlog_pos: Option<u64>,
}

impl MetaStore {
    /// Creates or replaces one database's encrypted backup configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid credential pairs, out-of-range cadence,
    /// a missing database, or a `SQLite` write failure.
    pub fn upsert_backup_config(&self, config: &NewBackupConfig<'_>) -> Result<()> {
        if config.encrypted_access_key_id.is_some() != config.encrypted_secret_access_key.is_some()
        {
            bail!("backup access key ID and secret must be provided together");
        }
        let schedule = i64::try_from(config.schedule_minutes)
            .context("backup schedule exceeds SQLite range")?;
        self.connection
            .execute(
                "INSERT INTO backup_configs (\
                   db_id, bucket, prefix, endpoint, region, \
                   access_key_id_encrypted, secret_access_key_encrypted, \
                   schedule_minutes, enabled, retain_count, verify_restore, \
                   full_every, updated_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
                 ON CONFLICT(db_id) DO UPDATE SET \
                   bucket = excluded.bucket, prefix = excluded.prefix, \
                   endpoint = excluded.endpoint, region = excluded.region, \
                   access_key_id_encrypted = excluded.access_key_id_encrypted, \
                   secret_access_key_encrypted = excluded.secret_access_key_encrypted, \
                   schedule_minutes = excluded.schedule_minutes, \
                   enabled = excluded.enabled, \
                   retain_count = excluded.retain_count, \
                   verify_restore = excluded.verify_restore, \
                   full_every = excluded.full_every, \
                   updated_at = excluded.updated_at",
                params![
                    config.database_id,
                    config.bucket,
                    config.prefix,
                    config.endpoint,
                    config.region,
                    config.encrypted_access_key_id,
                    config.encrypted_secret_access_key,
                    schedule,
                    config.enabled,
                    i64::try_from(config.retain_count)
                        .context("backup retention exceeds SQLite range")?,
                    config.verify_restore,
                    i64::try_from(config.full_every)
                        .context("backup full cadence exceeds SQLite range")?,
                    config.now,
                ],
            )
            .context("failed to save backup configuration")?;
        Ok(())
    }

    /// Loads one database's backup configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or row decoding fails.
    pub fn backup_config(&self, database_id: &str) -> Result<Option<BackupConfigRecord>> {
        self.connection
            .query_row(
                "SELECT db_id, bucket, prefix, endpoint, region, \
                        access_key_id_encrypted, secret_access_key_encrypted, \
                        schedule_minutes, enabled, retain_count, updated_at, \
                        verify_restore, full_every \
                 FROM backup_configs WHERE db_id = ?1",
                [database_id],
                decode_backup_config,
            )
            .optional()
            .context("failed to load backup configuration")
    }

    /// Starts a durable backup run.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid kind, missing parent/database, or
    /// `SQLite` write failure.
    pub fn start_backup(&self, backup: &NewBackup<'_>) -> Result<()> {
        if !matches!(backup.kind, "full" | "incremental") {
            bail!("backup kind must be full or incremental");
        }
        self.connection
            .execute(
                "INSERT INTO backups (\
                   id, db_id, kind, parent_id, object_prefix, status, started_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6)",
                (
                    backup.id,
                    backup.database_id,
                    backup.kind,
                    backup.parent_id,
                    backup.object_prefix,
                    backup.started_at,
                ),
            )
            .context("failed to start backup")?;
        Ok(())
    }

    /// Completes or fails a durable backup run.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid status/counters, a missing run, or a
    /// `SQLite` write failure.
    pub fn finish_backup(
        &self,
        id: &str,
        status: &str,
        bytes: u64,
        object_count: u64,
        error: Option<&str>,
        completed_at: &str,
    ) -> Result<()> {
        if !matches!(status, "completed" | "error") {
            bail!("finished backup status must be completed or error");
        }
        let bytes = i64::try_from(bytes).context("backup bytes exceed SQLite range")?;
        let objects =
            i64::try_from(object_count).context("backup object count exceeds SQLite range")?;
        let changed = self
            .connection
            .execute(
                "UPDATE backups SET status = ?2, bytes = ?3, object_count = ?4, \
                   error = ?5, completed_at = ?6 WHERE id = ?1 AND status = 'running'",
                (id, status, bytes, objects, error, completed_at),
            )
            .context("failed to finish backup")?;
        if changed == 0 {
            bail!("running backup {id} does not exist");
        }
        Ok(())
    }

    /// Records the outcome of a post-backup restore validation.
    ///
    /// # Errors
    ///
    /// Returns an error when the backup row cannot be updated.
    pub fn record_backup_verification(
        &self,
        backup_id: &str,
        error: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let verified_at = if error.is_none() { Some(now) } else { None };
        self.connection
            .execute(
                "UPDATE backups SET verified_at = ?2, verify_error = ?3 WHERE id = ?1",
                (backup_id, verified_at, error),
            )
            .context("failed to record backup verification")?;
        Ok(())
    }

    /// Lists recent backup runs for one database.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or row decoding fails.
    pub fn backups(&self, database_id: &str, limit: u64) -> Result<Vec<BackupRecord>> {
        let limit = i64::try_from(limit).context("backup limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, db_id, kind, parent_id, object_prefix, status, \
                        bytes, object_count, error, started_at, completed_at, \
                        verified_at, verify_error \
                 FROM backups WHERE db_id = ?1 \
                 ORDER BY started_at DESC, id LIMIT ?2",
            )
            .context("failed to prepare backup list")?;
        statement
            .query_map((database_id, limit), decode_backup)
            .context("failed to list backups")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode backups")
    }

    /// Deletes one non-running backup's audit record (retention pruning).
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails; deleting a parent still
    /// referenced by another backup fails on the foreign key, so prune
    /// children before parents.
    pub fn delete_backup_record(&self, backup_id: &str) -> Result<()> {
        self.connection
            .execute(
                "DELETE FROM backups WHERE id = ?1 AND status != 'running'",
                [backup_id],
            )
            .context("failed to delete backup record")?;
        Ok(())
    }

    /// Returns the newest completed backup for incremental chaining.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or row decoding fails.
    pub fn latest_completed_backup(&self, database_id: &str) -> Result<Option<BackupRecord>> {
        self.connection
            .query_row(
                "SELECT id, db_id, kind, parent_id, object_prefix, status, \
                        bytes, object_count, error, started_at, completed_at, \
                        verified_at, verify_error \
                 FROM backups WHERE db_id = ?1 AND status = 'completed' \
                 ORDER BY started_at DESC, id DESC LIMIT 1",
                [database_id],
                decode_backup,
            )
            .optional()
            .context("failed to load latest completed backup")
    }

    /// Registers verified backup objects as a new, paused database.
    ///
    /// The source DSN is intentionally not present in backups. A restored
    /// replica is queryable side-by-side but remains detached from ingestion
    /// until an operator supplies source credentials.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid state, duplicate identity, out-of-range
    /// counters, or a transactional storage failure.
    pub fn register_restored_database(&self, restored: &RestoredDatabase<'_>) -> Result<()> {
        if !matches!(restored.effective_mode, "cdc" | "polling") {
            bail!("restored effective mode must be cdc or polling");
        }
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin restored database registration")?;
        transaction
            .execute(
                "INSERT INTO databases (\
                   id, name, mysql_dsn_encrypted, mode, effective_mode, state, \
                   probe_json, created_at, updated_at\
                 ) VALUES (?1, ?2, X'', 'paused', ?3, 'restored', ?4, ?5, ?5)",
                (
                    restored.id,
                    restored.name,
                    restored.effective_mode,
                    restored.probe_json,
                    restored.now,
                ),
            )
            .context("failed to register restored database")?;
        for table in restored.tables {
            let rows =
                i64::try_from(table.rows_synced).context("restored row count exceeds SQLite")?;
            transaction
                .execute(
                    "INSERT INTO tables (\
                       db_id, name, state, pk_json, cursor_column, sort_key_json, \
                       rows_synced, schema_version, soft_delete_column\
                     ) VALUES (?1, ?2, 'restored', ?3, ?4, ?5, ?6, ?7, ?8)",
                    (
                        restored.id,
                        table.name,
                        table.primary_key_json,
                        table.cursor_column,
                        table.sort_key_json,
                        rows,
                        i64::from(table.schema_version),
                        table.soft_delete_column,
                    ),
                )
                .with_context(|| format!("failed to register restored table {}", table.name))?;
        }
        if let Some(checkpoint) = &restored.checkpoint {
            if !matches!(checkpoint.kind, "gtid" | "filepos" | "polling") {
                bail!("restored checkpoint kind is invalid");
            }
            let position = checkpoint
                .binlog_pos
                .map(i64::try_from)
                .transpose()
                .context("restored binlog position exceeds SQLite")?;
            transaction
                .execute(
                    "INSERT INTO checkpoints (\
                       db_id, kind, gtid_set, binlog_file, binlog_pos, updated_at\
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (
                        restored.id,
                        checkpoint.kind,
                        checkpoint.gtid_set,
                        checkpoint.binlog_file,
                        position,
                        restored.now,
                    ),
                )
                .context("failed to register restored checkpoint")?;
        }
        transaction
            .commit()
            .context("failed to commit restored database registration")
    }
}

fn decode_backup_config(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupConfigRecord> {
    let schedule: i64 = row.get(7)?;
    Ok(BackupConfigRecord {
        database_id: row.get(0)?,
        bucket: row.get(1)?,
        prefix: row.get(2)?,
        endpoint: row.get(3)?,
        region: row.get(4)?,
        encrypted_access_key_id: row.get(5)?,
        encrypted_secret_access_key: row.get(6)?,
        retain_count: {
            let retain: i64 = row.get(9)?;
            u64::try_from(retain).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?
        },
        schedule_minutes: u64::try_from(schedule).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        enabled: row.get(8)?,
        verify_restore: row.get(11)?,
        full_every: {
            let full_every: i64 = row.get(12)?;
            u64::try_from(full_every).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    12,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })?
        },
        updated_at: row.get(10)?,
    })
}

fn decode_backup(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupRecord> {
    let bytes: i64 = row.get(6)?;
    let objects: i64 = row.get(7)?;
    Ok(BackupRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        kind: row.get(2)?,
        parent_id: row.get(3)?,
        object_prefix: row.get(4)?,
        status: row.get(5)?,
        bytes: u64::try_from(bytes).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        object_count: u64::try_from(objects).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        error: row.get(8)?,
        started_at: row.get(9)?,
        completed_at: row.get(10)?,
        verified_at: row.get(11)?,
        verify_error: row.get(12)?,
    })
}
