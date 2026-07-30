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
                   updated_at = ?9 \
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

    /// Changes the requested replication mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mode, a missing database, or a storage
    /// failure.
    pub fn set_database_mode(&self, id: &str, mode: &str, now: &str) -> Result<()> {
        validate_mode(mode)?;
        let effective = (mode == "paused").then_some("paused");
        let state = if mode == "paused" {
            "paused"
        } else {
            "created"
        };
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET mode = ?2, effective_mode = ?3, \
                   state = ?4, updated_at = ?5 WHERE id = ?1",
                (id, mode, effective, state, now),
            )
            .context("failed to update database mode")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
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
            reconcile_interval_seconds, created_at, updated_at \
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

fn validate_role(role: &str) -> Result<()> {
    if matches!(role, "admin" | "operator" | "viewer") {
        Ok(())
    } else {
        bail!("user role must be admin, operator, or viewer")
    }
}

fn validate_mode(mode: &str) -> Result<()> {
    if matches!(mode, "auto" | "cdc" | "polling" | "paused") {
        Ok(())
    } else {
        bail!("database mode must be auto, cdc, polling, or paused")
    }
}
