//! Pintail's SQLite-backed control plane.
//!
//! This crate stores configuration and replication metadata only. Analytical
//! row data belongs exclusively to `pintail-store`.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, Transaction};

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
