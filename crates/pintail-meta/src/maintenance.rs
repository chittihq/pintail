//! Keeping the control-plane file healthy over a deployment's life.
//!
//! Three duties, each small, all driven by the server on a timer:
//!
//! - **Checking.** A damaged page in this file is found by whichever read
//!   touches it first, and that read may be the dashboard's table list hours
//!   after the damage. `integrity_problems` looks on purpose, so damage is
//!   reported the moment it can be seen rather than the moment it bites.
//! - **Copying.** `backup_into` writes a consistent, compacted copy while the
//!   file stays in use. Rebuilding a damaged file by salvaging readable rows
//!   works, but a recent good copy is the recovery that loses nothing.
//! - **Pruning.** A replication cycle records one `sync_runs` row every few
//!   seconds, so the table grows by tens of thousands of rows a day per
//!   database and never shrinks. `prune_sync_runs` bounds it while keeping
//!   the rows that explain a copy. `prune_audit_log` does the same for
//!   the audit trail, which gains a row per query, by age alone and a
//!   bounded batch per transaction; `prune_dead_letters` does it for the
//!   dead-letter queue, which gains a row per event that cannot be decoded.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::params;

use crate::MetaStore;

/// Run kinds that record a copy or a repair. They are rare and they are the
/// history an operator reads after an incident, so they outlive the routine
/// replication cycles.
const DURABLE_RUN_KINDS: &str = "'snapshot', 'resnapshot', 'reconcile'";

impl MetaStore {
    /// What `SQLite` finds wrong with this file; empty when nothing is.
    ///
    /// `thorough` runs the full `integrity_check` (every index against its
    /// table) plus `foreign_key_check`; otherwise the cheaper `quick_check`,
    /// which still reads every page and catches the damage a torn write or a
    /// lost lock leaves behind.
    ///
    /// # Errors
    ///
    /// Returns an error when the check itself cannot run. A file too damaged
    /// to be checked reports that way, never as healthy.
    pub fn integrity_problems(&self, thorough: bool) -> Result<Vec<String>> {
        let pragma = if thorough {
            "PRAGMA integrity_check(100)"
        } else {
            "PRAGMA quick_check(100)"
        };
        let mut problems = {
            let mut statement = self
                .connection
                .prepare(pragma)
                .context("failed to prepare metadata integrity check")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .context("failed to run metadata integrity check")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read metadata integrity check")?
        };
        problems.retain(|line| line != "ok");
        if thorough {
            let mut statement = self
                .connection
                .prepare("PRAGMA foreign_key_check")
                .context("failed to prepare metadata foreign key check")?;
            let orphans = statement
                .query_map([], |row| {
                    Ok(format!(
                        "{} row {} has no parent in {}",
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?
                            .map_or_else(|| "?".to_owned(), |rowid| rowid.to_string()),
                        row.get::<_, String>(2)?,
                    ))
                })
                .context("failed to run metadata foreign key check")?
                .take(100)
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("failed to read metadata foreign key check")?;
            problems.extend(orphans);
        }
        Ok(problems)
    }

    /// Writes a consistent copy of this file to `target`, replacing any file
    /// already there only once the new copy is complete.
    ///
    /// The copy is taken inside one read transaction, so it is a single
    /// point in time even while replication writes, and it is compacted: no
    /// free pages, no WAL beside it. It is written under a temporary name
    /// and renamed, so a crash mid-copy never leaves a truncated file where
    /// the last good backup was.
    ///
    /// # Errors
    ///
    /// Returns an error when the copy cannot be written or renamed.
    pub fn backup_into(&self, target: &Path) -> Result<()> {
        let Some(file_name) = target.file_name() else {
            bail!(
                "metadata backup target {} has no file name",
                target.display()
            );
        };
        let mut partial_name = file_name.to_os_string();
        partial_name.push(".partial");
        let partial = target.with_file_name(partial_name);
        // VACUUM INTO refuses an existing file; a leftover from a copy a
        // crash cut short is garbage by definition.
        match std::fs::remove_file(&partial) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to clear stale backup {}", partial.display())
                });
            }
        }
        let Some(partial_text) = partial.to_str() else {
            bail!("metadata backup path {} is not UTF-8", partial.display());
        };
        self.connection
            .execute("VACUUM INTO ?1", [partial_text])
            .with_context(|| format!("failed to write metadata backup {}", partial.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to restrict {}", partial.display()))?;
        }
        std::fs::rename(&partial, target).with_context(|| {
            format!(
                "failed to move metadata backup into place at {}",
                target.display()
            )
        })
    }

    /// Deletes finished `sync_runs` rows older than their retention and
    /// returns how many went.
    ///
    /// Routine replication cycles that succeeded are kept `cycles_since`
    /// onward; everything else finished - failures, copies, repairs - is
    /// kept `history_since` onward. A run still marked `running` is never
    /// deleted: it is either live or evidence of a process that died
    /// mid-run. Both bounds are RFC 3339 UTC timestamps, compared as text
    /// exactly as the rows are written.
    ///
    /// # Errors
    ///
    /// Returns an error when the delete fails.
    pub fn prune_sync_runs(&self, cycles_since: &str, history_since: &str) -> Result<u64> {
        let cycles = self
            .connection
            .execute(
                &format!(
                    "DELETE FROM sync_runs WHERE status = 'completed' \
                     AND kind NOT IN ({DURABLE_RUN_KINDS}) AND started_at < ?1"
                ),
                [cycles_since],
            )
            .context("failed to prune replication cycle history")?;
        let history = self
            .connection
            .execute(
                "DELETE FROM sync_runs WHERE status <> 'running' AND started_at < ?1",
                [history_since],
            )
            .context("failed to prune sync run history")?;
        Ok(u64::try_from(cycles + history).unwrap_or(u64::MAX))
    }

    /// Deletes audit events written before `before`, at most `batch_rows`
    /// per transaction, until none is left, and reports what went.
    ///
    /// Each batch is its own short write transaction, and none is held
    /// between batches, so the audit writer and request handlers waiting on
    /// the write lock get it between any two. `between` runs after every
    /// batch that may have left rows behind, with the number that batch
    /// removed, before the next begins: the caller's chance to give way.
    ///
    /// `before` is compared as text with `created_at`, which is written as
    /// RFC 3339 UTC. Give it as `YYYY-MM-DDTHH:MM:SS` with no fraction or
    /// offset: a row from that very second then compares greater and is
    /// kept, whether it was written with `Z` or `+00:00`.
    ///
    /// The deletes are journal writes: they move no write generation.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch cannot be deleted; batches committed
    /// before it stay deleted.
    pub fn prune_audit_log(
        &self,
        before: &str,
        batch_rows: u64,
        between: impl FnMut(u64),
    ) -> Result<AgePrune> {
        self.prune_by_age(AgedTable::AuditLog, before, batch_rows, between)
            .context("failed to prune the audit log")
    }

    /// Deletes dead letters recorded before `before`, at most `batch_rows`
    /// per transaction, until none is left, and reports what went.
    ///
    /// Batches, `between` and the form of `before` are as for
    /// [`Self::prune_audit_log`]. Unlike it, each batch is an ordinary
    /// commit that moves the write generation, as recording or discarding a
    /// dead letter does. A dead letter carries no state of its own: the
    /// list, its counts and the exported gauge are all read from these
    /// rows, so a pruned letter leaves them all at once.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch cannot be deleted; batches committed
    /// before it stay deleted.
    pub fn prune_dead_letters(
        &self,
        before: &str,
        batch_rows: u64,
        between: impl FnMut(u64),
    ) -> Result<AgePrune> {
        self.prune_by_age(AgedTable::DeadLetters, before, batch_rows, between)
            .context("failed to prune the dead-letter queue")
    }

    fn prune_by_age(
        &self,
        table: AgedTable,
        before: &str,
        batch_rows: u64,
        mut between: impl FnMut(u64),
    ) -> Result<AgePrune> {
        let limit = i64::try_from(batch_rows.max(1)).unwrap_or(i64::MAX);
        let statement = match table {
            AgedTable::AuditLog => {
                "DELETE FROM audit_log WHERE rowid IN (\
                   SELECT rowid FROM audit_log WHERE created_at < ?1 \
                   ORDER BY created_at LIMIT ?2)"
            }
            AgedTable::DeadLetters => {
                "DELETE FROM dlq WHERE rowid IN (\
                   SELECT rowid FROM dlq WHERE created_at < ?1 \
                   ORDER BY created_at LIMIT ?2)"
            }
        };
        let batch = |connection: &rusqlite::Connection| -> rusqlite::Result<usize> {
            let transaction = connection.unchecked_transaction()?;
            let removed = transaction.execute(statement, params![before, limit])?;
            transaction.commit()?;
            Ok(removed)
        };
        let mut outcome = AgePrune::default();
        loop {
            let removed = match table {
                AgedTable::AuditLog => self.journal_write(batch)?,
                AgedTable::DeadLetters => batch(&self.connection)?,
            };
            let removed = u64::try_from(removed).unwrap_or(u64::MAX);
            outcome.batches += 1;
            outcome.removed = outcome.removed.saturating_add(removed);
            if removed < batch_rows.max(1) {
                return Ok(outcome);
            }
            between(removed);
        }
    }
}

/// The tables pruned by age alone.
#[derive(Clone, Copy)]
enum AgedTable {
    AuditLog,
    DeadLetters,
}

/// What one pass of [`MetaStore::prune_audit_log`] or
/// [`MetaStore::prune_dead_letters`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgePrune {
    /// Rows deleted.
    pub removed: u64,
    /// Transactions it took, the last of which found fewer than a batch.
    pub batches: u64,
}
