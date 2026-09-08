# Read copies restored from backups

Status: design proposal. The only implementation in this slice is restored-data
age reporting. There is no second-process manager or automatic refresh loop.

A second Pintail process can serve a verified backup without participating in
replication. This is the smallest useful read tier when callers can tolerate
backup-cadence staleness. It adds read capacity and a separate failure domain for
queries, but it is not a standby and cannot take over the primary's role.

## Restore and publish

The primary continues creating its normal full and incremental backups. A read
copy needs read-only access to the backup destination and every object referenced
by the selected manifest, including objects reused from earlier backups. Retention
must protect a manifest and its complete reachable object set while a restore
uses it. A missing parent object or a checksum failure leaves the installed copy
serving its previous data.

A refresh controller would poll completed backup manifests on a configured
schedule. It would choose a new backup identity, restore through the existing
checksum-verifying restore path in `crates/pintail-api/src/backup.rs` into a new
directory, and validate the restored catalog and table manifests. It must never
replace files under a live query. After validation it would publish a new catalog
and storage generation atomically; running queries finish on the old generation,
which is removed only after its last reader releases it. Repeating the same
manifest is a no-op. An interrupted refresh leaves the previous generation valid;
on restart, uncommitted staging directories are recognized and recovered or
removed by the controller that created them.

The second process would use its own data directory and control plane. Restore
registration already omits source credentials and records paused replication.
A read-only process role still needs explicit enforcement before this design can
ship: refuse local writes, source registration, replication resume, snapshot and
other data-changing API operations on that process. Authentication, TLS and
read-only API keys are configured for the copy independently. This proposal does
not implement that role or route traffic between processes.

## Freshness contract

The shipped `restored_backup_created_at` field preserves the installed backup
manifest's creation timestamp through restarts. Database list/detail and restore
responses include `data_age_seconds`; `/metrics` includes
`pintail_restored_data_age_seconds{database="..."}` while the database remains in
`restored` state. Age is computed at request time as current UTC minus that
manifest timestamp, clamped to zero for a clock ahead of the reader. A restore
performed today from an old backup therefore reports an old copy immediately.
A pre-existing restore with no recorded timestamp reports an unknown age (`null`
in the API and no metric sample), not zero.

This is the age of the installed backup manifest, not source transaction lag.
It excludes replication lag already present in the primary and the time between
table capture and manifest publication. It does not establish a database-wide
transaction snapshot across independently captured tables. A refresh failure
keeps the old timestamp, so age continues rising. Leaving `restored` state stops
this metric; a live replication stream has its own lag measurement. Renaming or
restarting a restored database does not reset its age.

A future controller would expose refresh failures, selected backup identity and
next scheduled refresh separately. It should permit an age ceiling that refuses
new queries once exceeded, rather than presenting a stale result as fresh.

## Boundaries and a proper standby

This design offers no failover, promotion, write path, read-after-write guarantee
or continuous replication. Its freshness is bounded by backup cadence, capture,
transfer and validation time; failed refreshes make that interval longer. A query
router, discovery mechanism and generation publication protocol are additional
work. A copy's health endpoint alone cannot establish an acceptable data age.

A proper standby would require an ordered WAL shipping protocol, durable received
and applied positions, replay compatibility and retention, gap detection and a
resynchronization path. Promotion additionally needs an explicit authority change:
fencing the old writer, choosing and recording a promotion boundary, reconciling
source CDC ownership/checkpoints, routing clients, and preventing split brain.
Those changes require their own design and failure testing. Backup restores do
not supply them, and this proposal does not add point-in-time recovery.

The owner can choose this read tier if coarse freshness and read availability are
useful independently of failover. If continuity of ingestion or promotion is the
requirement, a scheduled backup restore is insufficient.
