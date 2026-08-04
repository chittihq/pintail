use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension as _, params};

use crate::MetaStore;

/// Durable dashboard user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRecord {
    pub id: String,
    pub email: String,
    pub argon2_hash: String,
    pub role: String,
    pub enabled: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

/// Durable source-database configuration and status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseRecord {
    pub id: String,
    pub name: String,
    pub encrypted_dsn: Vec<u8>,
    pub mode: String,
    pub effective_mode: Option<String>,
    pub state: String,
    pub probe_json: Option<String>,
    pub include_tables: Option<String>,
    pub exclude_tables: Option<String>,
    pub poll_interval_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub created_at: String,
    pub updated_at: String,
    /// Keyless-table replication policy: `quarantine`, `auto_resync`, or
    /// `reject`.
    pub keyless_policy: String,
}

/// Mutable source-database settings.
pub struct DatabaseUpdate<'a> {
    pub name: &'a str,
    pub encrypted_dsn: Option<&'a [u8]>,
    pub mode: &'a str,
    pub include_tables: Option<&'a str>,
    pub exclude_tables: Option<&'a str>,
    pub poll_interval_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub keyless_policy: &'a str,
    pub now: &'a str,
}

/// Durable source-table status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRecord {
    pub database_id: String,
    pub name: String,
    pub state: String,
    pub primary_key_json: Option<String>,
    pub cursor_column: Option<String>,
    pub sort_key_json: Option<String>,
    pub rows_synced: u64,
    pub last_error: Option<String>,
    pub last_reconcile_at: Option<String>,
    pub schema_version: u32,
    pub orphaned_at: Option<String>,
    pub soft_delete_column: Option<String>,
}

/// Durable database-scoped API key metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKeyRecord {
    pub id: String,
    pub database_id: String,
    pub name: String,
    pub sha256: Vec<u8>,
    pub mysql_native_password_hash: Option<Vec<u8>>,
    pub caching_sha2_password_hash: Option<Vec<u8>>,
    pub enabled: bool,
    pub scopes_json: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
}

/// Values for one newly generated API key.
pub struct NewApiKey<'a> {
    pub id: &'a str,
    pub database_id: &'a str,
    pub name: &'a str,
    pub sha256: &'a [u8],
    pub mysql_native_password_hash: Option<&'a [u8]>,
    pub caching_sha2_password_hash: Option<&'a [u8]>,
    pub scopes_json: &'a str,
    pub expires_at: Option<&'a str>,
    pub now: &'a str,
}

/// One sync activity record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncRunRecord {
    pub id: String,
    pub database_id: String,
    pub table_name: Option<String>,
    pub kind: String,
    pub status: String,
    pub rows: u64,
    pub bytes: u64,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub started_at: String,
}

/// One dead-letter record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DlqRecord {
    pub id: String,
    pub database_id: String,
    pub table_name: Option<String>,
    pub event_json: String,
    pub error: String,
    pub created_at: String,
}

impl MetaStore {
    /// Returns the number of configured users.
    ///
    /// # Errors
    ///
    /// Returns an error when the user table cannot be counted.
    pub fn user_count(&self) -> Result<u64> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .context("failed to count users")?;
        u64::try_from(count).context("user count is negative")
    }

    /// Creates one dashboard user.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role, duplicate identity, or storage
    /// failure.
    pub fn create_user(
        &self,
        id: &str,
        email: &str,
        argon2_hash: &str,
        role: &str,
        now: &str,
    ) -> Result<()> {
        validate_role(role)?;
        self.connection
            .execute(
                "INSERT INTO users (id, email, argon2_hash, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (id, email, argon2_hash, role, now),
            )
            .context("failed to create user")?;
        Ok(())
    }

    /// Returns a user by case-insensitive email.
    ///
    /// # Errors
    ///
    /// Returns an error when the user record cannot be read or decoded.
    pub fn user_by_email(&self, email: &str) -> Result<Option<UserRecord>> {
        self.connection
            .query_row(
                "SELECT id, email, argon2_hash, role, enabled, created_at, last_login_at \
                 FROM users WHERE email = ?1 COLLATE NOCASE",
                [email],
                decode_user,
            )
            .optional()
            .context("failed to read user")
    }

    /// Lists users in email order.
    ///
    /// # Errors
    ///
    /// Returns an error when user records cannot be read or decoded.
    pub fn users(&self) -> Result<Vec<UserRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, email, argon2_hash, role, enabled, created_at, last_login_at \
                 FROM users ORDER BY email COLLATE NOCASE",
            )
            .context("failed to prepare user query")?;
        statement
            .query_map([], decode_user)
            .context("failed to query users")?
            .collect::<rusqlite::Result<_>>()
            .context("failed to decode users")
    }

    /// Records a successful login.
    ///
    /// # Errors
    ///
    /// Returns an error when the user record cannot be updated.
    pub fn touch_user_login(&self, id: &str, now: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to update user login")?;
        Ok(())
    }

    /// Lists configured source databases.
    ///
    /// # Errors
    ///
    /// Returns an error when database records cannot be read or decoded.
    pub fn databases(&self) -> Result<Vec<DatabaseRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{} ORDER BY name COLLATE NOCASE",
                database_select_sql()
            ))
            .context("failed to prepare database query")?;
        statement
            .query_map([], decode_database)
            .context("failed to query databases")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode databases")
    }

    /// Returns one configured source database.
    ///
    /// # Errors
    ///
    /// Returns an error when the database record cannot be read or decoded.
    pub fn database(&self, id: &str) -> Result<Option<DatabaseRecord>> {
        self.connection
            .query_row(
                &format!("{} WHERE id = ?1", database_select_sql()),
                [id],
                decode_database,
            )
            .optional()
            .context("failed to read database")
    }

    /// Updates operator-editable database settings.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid settings, a missing database, or a storage
    /// failure.
    pub fn update_database(&self, id: &str, update: &DatabaseUpdate<'_>) -> Result<()> {
        validate_mode(update.mode)?;
        validate_keyless_policy(update.keyless_policy)?;
        let poll_interval = i64::try_from(update.poll_interval_seconds)
            .context("poll interval exceeds SQLite range")?;
        let reconcile_interval = i64::try_from(update.reconcile_interval_seconds)
            .context("reconcile interval exceeds SQLite range")?;
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET \
                   name = ?2, \
                   mysql_dsn_encrypted = COALESCE(?3, mysql_dsn_encrypted), \
                   mode = ?4, include_tables = ?5, exclude_tables = ?6, \
                   poll_interval_seconds = ?7, reconcile_interval_seconds = ?8, \
                   updated_at = ?9, keyless_policy = ?10 \
                 WHERE id = ?1",
                params![
                    id,
                    update.name,
                    update.encrypted_dsn,
                    update.mode,
                    update.include_tables,
                    update.exclude_tables,
                    poll_interval,
                    reconcile_interval,
                    update.now,
                    update.keyless_policy,
                ],
            )
            .context("failed to update database")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Persists the latest source probe and effective mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mode, a missing database, or a storage
    /// failure.
    pub fn update_database_probe(
        &self,
        id: &str,
        probe_json: &str,
        effective_mode: &str,
        now: &str,
    ) -> Result<()> {
        if !matches!(effective_mode, "cdc" | "polling") {
            bail!("effective database mode must be cdc or polling");
        }
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET probe_json = ?2, effective_mode = ?3, \
                   state = 'probed', updated_at = ?4 WHERE id = ?1",
                (id, probe_json, effective_mode, now),
            )
            .context("failed to persist database probe")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Replaces only the stored probe report, leaving state and modes
    /// untouched. The CDC runner uses this when live DDL changes the source
    /// inventory (e.g. auto-including a newly created table): every consumer
    /// of `probe_json` — the supervisor's target set and the query engine's
    /// catalog — must see the new table without disturbing replication state.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing database or a storage failure.
    pub fn refresh_database_probe_json(&self, id: &str, probe_json: &str, now: &str) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET probe_json = ?2, updated_at = ?3 WHERE id = ?1",
                (id, probe_json, now),
            )
            .context("failed to refresh database probe")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Changes the requested replication mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mode, a missing database, or a storage
    /// failure.
    pub fn set_database_mode(&self, id: &str, mode: &str, now: &str) -> Result<()> {
        validate_mode(mode)?;
        let changed = match mode {
            "paused" => self
                .connection
                .execute(
                    "UPDATE databases SET mode = 'paused', effective_mode = 'paused', \
                       state = 'paused', updated_at = ?2 WHERE id = ?1",
                    (id, now),
                )
                .context("failed to pause database")?,
            // `auto` means "follow the probe recommendation": a live
            // effective mode stays live until the next probe re-derives it,
            // while leaving pause via `auto` waits for that probe.
            "auto" => self
                .connection
                .execute(
                    "UPDATE databases SET mode = 'auto', \
                       effective_mode = CASE WHEN effective_mode = 'paused' \
                         THEN NULL ELSE effective_mode END, \
                       updated_at = ?2 WHERE id = ?1",
                    (id, now),
                )
                .context("failed to update database mode")?,
            // An explicit cdc/polling switch takes effect immediately and
            // must keep replication ALIVE: the supervisor only schedules
            // databases whose state is streaming/polling/error, so an
            // active (or paused) database transitions to the new mode's
            // running state instead of being reset to 'created' — that
            // reset silently stopped replication until a manual re-probe
            // (found by the e2e control-plane gate, 2026-08-03).
            explicit => self
                .connection
                .execute(
                    "UPDATE databases SET mode = ?2, effective_mode = ?2, \
                       state = CASE \
                         WHEN state IN ('streaming', 'polling', 'error', 'paused') \
                         THEN (CASE ?2 WHEN 'polling' THEN 'polling' ELSE 'streaming' END) \
                         ELSE state \
                       END, \
                       updated_at = ?3 WHERE id = ?1",
                    (id, explicit, now),
                )
                .context("failed to update database mode")?,
        };
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Publishes a completed snapshot handoff state.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid effective mode, a missing database, or
    /// a storage failure.
    pub fn set_database_replication_state(
        &self,
        id: &str,
        effective_mode: &str,
        now: &str,
    ) -> Result<()> {
        let (database_state, table_state) = match effective_mode {
            "cdc" => ("streaming", "streaming"),
            "polling" => ("polling", "polling"),
            _ => bail!("effective database mode must be cdc or polling"),
        };
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin replication-state update")?;
        let changed = transaction
            .execute(
                "UPDATE databases SET effective_mode = ?2, state = ?3, updated_at = ?4 \
                 WHERE id = ?1",
                (id, effective_mode, database_state, now),
            )
            .context("failed to update database replication state")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        transaction
            .execute(
                "UPDATE tables SET state = ?2, last_error = NULL \
                 WHERE db_id = ?1 AND state NOT IN ('excluded', 'needs_resync')",
                (id, table_state),
            )
            .context("failed to update table replication states")?;
        transaction
            .commit()
            .context("failed to commit replication-state update")
    }

    /// Records a database-level API job failure.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be updated.
    pub fn fail_database_job(&self, id: &str, error: &str, now: &str) -> Result<()> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin database failure update")?;
        let changed = transaction
            .execute(
                "UPDATE databases SET state = 'error', updated_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to record database job failure")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        transaction
            .execute(
                "UPDATE tables SET state = 'error', last_error = ?2 \
                 WHERE db_id = ?1 AND state NOT IN ('excluded', 'needs_resync')",
                (id, error),
            )
            .context("failed to record table job failure")?;
        transaction
            .commit()
            .context("failed to commit database failure update")
    }

    /// Deletes one database and its cascading control-plane records.
    ///
    /// # Errors
    ///
    /// Returns an error when the database record cannot be deleted.
    pub fn delete_database(&self, id: &str) -> Result<bool> {
        self.connection
            .execute("DELETE FROM databases WHERE id = ?1", [id])
            .map(|changed| changed == 1)
            .context("failed to delete database")
    }

    /// Lists durable table status for one database.
    ///
    /// # Errors
    ///
    /// Returns an error when table records cannot be read or decoded.
    pub fn tables(&self, database_id: &str) -> Result<Vec<TableRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT db_id, name, state, pk_json, cursor_column, sort_key_json, \
                        rows_synced, last_error, last_reconcile_at, schema_version, \
                        orphaned_at, soft_delete_column \
                 FROM tables WHERE db_id = ?1 ORDER BY name COLLATE NOCASE",
            )
            .context("failed to prepare table query")?;
        statement
            .query_map([database_id], decode_table)
            .context("failed to query tables")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode tables")
    }

    /// Configures an optional source soft-delete column.
    ///
    /// # Errors
    ///
    /// Returns an error when the table is absent or cannot be updated.
    pub fn set_table_soft_delete_column(
        &self,
        database_id: &str,
        table_name: &str,
        column: Option<&str>,
    ) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE tables SET soft_delete_column = ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, column),
            )
            .context("failed to configure soft-delete column")?;
        if changed == 0 {
            bail!("table {database_id}.{table_name} does not exist");
        }
        Ok(())
    }

    /// Persists one hash-only database API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the database is absent or key metadata cannot be
    /// stored.
    pub fn create_api_key(&self, key: &NewApiKey<'_>) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO api_keys (\
                   id, db_id, name, sha256, mysql_native_password_hash, \
                   caching_sha2_password_hash, scopes_json, expires_at, created_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    key.id,
                    key.database_id,
                    key.name,
                    key.sha256,
                    key.mysql_native_password_hash,
                    key.caching_sha2_password_hash,
                    key.scopes_json,
                    key.expires_at,
                    key.now,
                ],
            )
            .context("failed to create API key")?;
        Ok(())
    }

    /// Lists API keys for one database without exposing their secret.
    ///
    /// # Errors
    ///
    /// Returns an error when API-key records cannot be read or decoded.
    pub fn api_keys(&self, database_id: &str) -> Result<Vec<ApiKeyRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{} WHERE db_id = ?1 ORDER BY created_at DESC, id",
                api_key_select_sql()
            ))
            .context("failed to prepare API-key query")?;
        statement
            .query_map([database_id], decode_api_key)
            .context("failed to query API keys")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode API keys")
    }

    /// Finds an API key by its SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the API-key record cannot be read or decoded.
    pub fn api_key_by_sha256(&self, sha256: &[u8]) -> Result<Option<ApiKeyRecord>> {
        self.connection
            .query_row(
                &format!("{} WHERE sha256 = ?1", api_key_select_sql()),
                [sha256],
                decode_api_key,
            )
            .optional()
            .context("failed to read API key")
    }

    /// Enables or disables an API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is absent or cannot be updated.
    pub fn set_api_key_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE api_keys SET enabled = ?2 WHERE id = ?1",
                (id, enabled),
            )
            .context("failed to update API key")?;
        if changed == 0 {
            bail!("API key {id} does not exist");
        }
        Ok(())
    }

    /// Records successful API-key authentication.
    ///
    /// # Errors
    ///
    /// Returns an error when the key cannot be updated.
    pub fn touch_api_key(&self, id: &str, now: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to update API-key usage")?;
        Ok(())
    }

    /// Deletes one API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key cannot be deleted.
    pub fn delete_api_key(&self, id: &str) -> Result<bool> {
        self.connection
            .execute("DELETE FROM api_keys WHERE id = ?1", [id])
            .map(|changed| changed == 1)
            .context("failed to delete API key")
    }

    /// Starts one durable activity record.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent database is absent or the run cannot
    /// be stored.
    pub fn start_sync_run(
        &self,
        id: &str,
        database_id: &str,
        table_name: Option<&str>,
        kind: &str,
        now: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO sync_runs (\
                   id, db_id, table_name, kind, status, started_at\
                 ) VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
                (id, database_id, table_name, kind, now),
            )
            .context("failed to start sync run")?;
        Ok(())
    }

    /// Completes one durable activity record.
    ///
    /// # Errors
    ///
    /// Returns an error when counters exceed `SQLite`'s range, the run is
    /// absent, or it cannot be updated.
    pub fn finish_sync_run(
        &self,
        id: &str,
        status: &str,
        rows: u64,
        bytes: u64,
        duration_ms: u64,
        error: Option<&str>,
    ) -> Result<()> {
        let rows = i64::try_from(rows).context("sync rows exceed SQLite range")?;
        let bytes = i64::try_from(bytes).context("sync bytes exceed SQLite range")?;
        let duration = i64::try_from(duration_ms).context("sync duration exceeds SQLite range")?;
        let changed = self
            .connection
            .execute(
                "UPDATE sync_runs SET status = ?2, rows = ?3, bytes = ?4, \
                   duration_ms = ?5, error = ?6 WHERE id = ?1",
                (id, status, rows, bytes, duration, error),
            )
            .context("failed to complete sync run")?;
        if changed == 0 {
            bail!("sync run {id} does not exist");
        }
        Ok(())
    }

    /// Lists recent sync activity, optionally limited to one database.
    ///
    /// # Errors
    ///
    /// Returns an error when activity records cannot be read or decoded.
    pub fn sync_runs(&self, database_id: Option<&str>, limit: u64) -> Result<Vec<SyncRunRecord>> {
        let limit = i64::try_from(limit).context("sync-run limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, db_id, table_name, kind, status, rows, bytes, duration_ms, \
                        error, started_at \
                 FROM sync_runs \
                 WHERE (?1 IS NULL OR db_id = ?1) \
                 ORDER BY started_at DESC, id LIMIT ?2",
            )
            .context("failed to prepare sync-run query")?;
        statement
            .query_map((database_id, limit), decode_sync_run)
            .context("failed to query sync runs")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode sync runs")
    }

    /// Lists recent dead-letter records, optionally limited to one database.
    ///
    /// # Errors
    ///
    /// Returns an error when dead-letter records cannot be read or decoded.
    pub fn dlq_records(&self, database_id: Option<&str>, limit: u64) -> Result<Vec<DlqRecord>> {
        let limit = i64::try_from(limit).context("DLQ limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, db_id, table_name, event_json, error, created_at \
                 FROM dlq WHERE (?1 IS NULL OR db_id = ?1) \
                 ORDER BY created_at DESC, id LIMIT ?2",
            )
            .context("failed to prepare DLQ query")?;
        statement
            .query_map((database_id, limit), decode_dlq)
            .context("failed to query DLQ records")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode DLQ records")
    }

    /// Loads one dead-letter record by ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or row decoding fails.
    pub fn dlq_record(&self, id: &str) -> Result<Option<DlqRecord>> {
        self.connection
            .query_row(
                "SELECT id, db_id, table_name, event_json, error, created_at \
                 FROM dlq WHERE id = ?1",
                [id],
                decode_dlq,
            )
            .optional()
            .context("failed to load DLQ record")
    }

    /// Discards one dead-letter record.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be deleted.
    pub fn delete_dlq_record(&self, id: &str) -> Result<bool> {
        self.connection
            .execute("DELETE FROM dlq WHERE id = ?1", [id])
            .map(|changed| changed == 1)
            .context("failed to delete DLQ record")
    }
}

fn decode_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(0)?,
        email: row.get(1)?,
        argon2_hash: row.get(2)?,
        role: row.get(3)?,
        enabled: row.get(4)?,
        created_at: row.get(5)?,
        last_login_at: row.get(6)?,
    })
}

fn database_select_sql() -> &'static str {
    "SELECT id, name, mysql_dsn_encrypted, mode, effective_mode, state, probe_json, \
            include_tables, exclude_tables, poll_interval_seconds, \
            reconcile_interval_seconds, created_at, updated_at, keyless_policy \
     FROM databases"
}

fn decode_database(row: &rusqlite::Row<'_>) -> rusqlite::Result<DatabaseRecord> {
    let poll_interval: i64 = row.get(9)?;
    let reconcile_interval: i64 = row.get(10)?;
    Ok(DatabaseRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        encrypted_dsn: row.get(2)?,
        mode: row.get(3)?,
        effective_mode: row.get(4)?,
        state: row.get(5)?,
        probe_json: row.get(6)?,
        include_tables: row.get(7)?,
        exclude_tables: row.get(8)?,
        poll_interval_seconds: u64::try_from(poll_interval).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        reconcile_interval_seconds: u64::try_from(reconcile_interval).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        keyless_policy: row.get(13)?,
    })
}

fn decode_table(row: &rusqlite::Row<'_>) -> rusqlite::Result<TableRecord> {
    let rows_synced: i64 = row.get(6)?;
    let schema_version: i64 = row.get(9)?;
    Ok(TableRecord {
        database_id: row.get(0)?,
        name: row.get(1)?,
        state: row.get(2)?,
        primary_key_json: row.get(3)?,
        cursor_column: row.get(4)?,
        sort_key_json: row.get(5)?,
        rows_synced: u64::try_from(rows_synced).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        last_error: row.get(7)?,
        last_reconcile_at: row.get(8)?,
        schema_version: u32::try_from(schema_version).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        orphaned_at: row.get(10)?,
        soft_delete_column: row.get(11)?,
    })
}

fn api_key_select_sql() -> &'static str {
    "SELECT id, db_id, name, sha256, mysql_native_password_hash, \
            caching_sha2_password_hash, enabled, \
            scopes_json, expires_at, last_used_at, created_at FROM api_keys"
}

fn decode_api_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyRecord> {
    Ok(ApiKeyRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        name: row.get(2)?,
        sha256: row.get(3)?,
        mysql_native_password_hash: row.get(4)?,
        caching_sha2_password_hash: row.get(5)?,
        enabled: row.get(6)?,
        scopes_json: row.get(7)?,
        expires_at: row.get(8)?,
        last_used_at: row.get(9)?,
        created_at: row.get(10)?,
    })
}

fn decode_sync_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncRunRecord> {
    let rows: i64 = row.get(5)?;
    let bytes: i64 = row.get(6)?;
    let duration: Option<i64> = row.get(7)?;
    Ok(SyncRunRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        table_name: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        rows: decode_u64(5, rows)?,
        bytes: decode_u64(6, bytes)?,
        duration_ms: duration.map(|value| decode_u64(7, value)).transpose()?,
        error: row.get(8)?,
        started_at: row.get(9)?,
    })
}

fn decode_dlq(row: &rusqlite::Row<'_>) -> rusqlite::Result<DlqRecord> {
    Ok(DlqRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        table_name: row.get(2)?,
        event_json: row.get(3)?,
        error: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn decode_u64(index: usize, value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn validate_role(role: &str) -> Result<()> {
    if matches!(role, "admin" | "operator" | "viewer") {
        Ok(())
    } else {
        bail!("user role must be admin, operator, or viewer")
    }
}

/// Rejects unknown keyless-table policies.
fn validate_keyless_policy(policy: &str) -> Result<()> {
    if !matches!(policy, "quarantine" | "auto_resync" | "reject") {
        bail!("keyless policy must be quarantine, auto_resync, or reject");
    }
    Ok(())
}

fn validate_mode(mode: &str) -> Result<()> {
    if matches!(mode, "auto" | "cdc" | "polling" | "paused") {
        Ok(())
    } else {
        bail!("database mode must be auto, cdc, polling, or paused")
    }
}
