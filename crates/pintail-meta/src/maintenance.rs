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
//!   bounded batch per transaction.

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
        mut between: impl FnMut(u64),
    ) -> Result<AuditPrune> {
        let limit = i64::try_from(batch_rows.max(1)).unwrap_or(i64::MAX);
        let mut outcome = AuditPrune::default();
        loop {
            let removed = self
                .journal_write(|connection| {
                    let transaction = connection.unchecked_transaction()?;
                    let removed = transaction.execute(
                        "DELETE FROM audit_log WHERE rowid IN (\
                           SELECT rowid FROM audit_log WHERE created_at < ?1 \
                           ORDER BY created_at LIMIT ?2)",
                        params![before, limit],
                    )?;
                    transaction.commit()?;
                    Ok(removed)
                })
                .context("failed to prune the audit log")?;
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

/// What one pass of [`MetaStore::prune_audit_log`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuditPrune {
    /// Audit events deleted.
    pub removed: u64,
    /// Transactions it took, the last of which found fewer than a batch.
    pub batches: u64,
}
