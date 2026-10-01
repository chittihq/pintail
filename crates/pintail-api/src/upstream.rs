//! Reconciles the source's table inventory against the catalog.
//!
//! Three lists describe one database's tables and nothing used to hold them
//! together: the source's base tables (the probe), the operator's selection
//! (the include and exclude lists), and the catalog (the `tables` rows that
//! replication actually streams). A catalog that loses rows for included
//! tables - a metadata recovery that kept the database but not every table -
//! simply stops mirroring them, with every cycle reporting success. This
//! module names the difference, lets an operator see and close it, and has
//! the supervisor close it on its own.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use mysql_async::Pool;
use pintail_meta::{DatabaseRecord, DatabaseUpdate, TableRecord};
use pintail_probe::{ProbeReport, RowCounts, SourceTable, probe_with};
use serde::{Deserialize, Serialize};

use crate::{
    ApiState, audit, auth::AuthPrincipal, error::ApiError, events::ApiEvent,
    snapshot::summarize_names,
};

/// How often the supervisor may start a catalog repair for one database.
///
/// A repair is a non-forced snapshot that copies exactly the missing tables,
/// so a successful one leaves nothing to repair. The gap exists for the
/// repair that cannot succeed - a source refusing the copy, a job slot held
/// for hours - which would otherwise start a snapshot every five seconds.
const CATALOG_REPAIR_GAP: Duration = Duration::from_secs(600);

/// Where one upstream (or formerly upstream) table stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum UpstreamStatus {
    /// On the source and in the catalog.
    Mirrored,
    /// On the source and selected, but absent from the catalog: drift.
    Missing,
    /// On the source, left out by the include or exclude list.
    NotIncluded,
    /// In the catalog, no longer on the source.
    DroppedUpstream,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UpstreamTable {
    name: String,
    status: UpstreamStatus,
    /// The source's row estimate, when the probe read one.
    estimated_rows: Option<u64>,
    rows_are_exact: bool,
    /// The catalog's state for the table, when it has a row.
    catalog_state: Option<String>,
    /// Named by the exclude list.
    excluded: bool,
}

#[derive(Debug, Default, Serialize)]
struct StatusCounts {
    mirrored: usize,
    missing: usize,
    not_included: usize,
    dropped_upstream: usize,
}

#[derive(Serialize)]
pub(crate) struct UpstreamTablesResponse {
    database_id: String,
    /// An empty include list selects every source table.
    include_all: bool,
    probed_at: String,
    counts: StatusCounts,
    tables: Vec<UpstreamTable>,
}

#[derive(Deserialize)]
pub(crate) struct AddUpstreamTablesRequest {
    tables: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct AddUpstreamTablesResponse {
    /// Names appended to the include list; empty when it selects everything.
    added_to_include: Vec<String>,
    /// Names taken off the exclude list, which would otherwise win.
    removed_from_exclude: Vec<String>,
    /// The snapshot copying them, or `None` when the job slot stayed busy and
    /// the supervisor's catalog repair copies them once it frees.
    run_id: Option<String>,
    state: &'static str,
}

/// The operator's selection, compared the way the snapshot compares it:
/// case-insensitively, with an empty include list meaning every table.
struct Selection {
    includes: BTreeSet<String>,
    excludes: BTreeSet<String>,
}

impl Selection {
    fn new(include: &[String], exclude: &[String]) -> Self {
        Self {
            includes: include
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect(),
            excludes: exclude
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect(),
        }
    }

    fn excluded(&self, name: &str) -> bool {
        self.excludes.contains(&name.to_ascii_lowercase())
    }

    fn selects(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        (self.includes.is_empty() || self.includes.contains(&name))
            && !self.excludes.contains(&name)
    }
}

/// Classifies every upstream table against the catalog and the selection,
/// then appends the catalog tables the source no longer has. Sorted by name.
pub(crate) fn classify(
    upstream: &[SourceTable],
    catalog: &[TableRecord],
    include: &[String],
    exclude: &[String],
) -> Vec<UpstreamTable> {
    let selection = Selection::new(include, exclude);
    let catalog_entry = |name: &str| {
        catalog
            .iter()
            .find(|record| record.name.eq_ignore_ascii_case(name))
    };
    let mut tables = upstream
        .iter()
        .map(|source| {
            let record = catalog_entry(&source.name);
            let status = if record.is_some() {
                UpstreamStatus::Mirrored
            } else if selection.selects(&source.name) {
                UpstreamStatus::Missing
            } else {
                UpstreamStatus::NotIncluded
            };
            UpstreamTable {
                name: source.name.clone(),
                status,
                estimated_rows: source.estimated_rows,
                rows_are_exact: source.rows_are_exact,
                catalog_state: record.map(|record| record.state.clone()),
                excluded: selection.excluded(&source.name),
            }
        })
        .collect::<Vec<_>>();
    for record in catalog {
        if !upstream
            .iter()
            .any(|source| source.name.eq_ignore_ascii_case(&record.name))
        {
            tables.push(UpstreamTable {
                name: record.name.clone(),
                status: UpstreamStatus::DroppedUpstream,
                estimated_rows: None,
                rows_are_exact: false,
                catalog_state: Some(record.state.clone()),
                excluded: selection.excluded(&record.name),
            });
        }
    }
    tables.sort_by(|left, right| left.name.cmp(&right.name));
    tables
}

/// Selected source tables the catalog has no row for.
pub(crate) fn missing_from_catalog(
    upstream: &[SourceTable],
    catalog: &[TableRecord],
    include: &[String],
    exclude: &[String],
) -> Vec<String> {
    classify(upstream, catalog, include, exclude)
        .into_iter()
        .filter(|table| table.status == UpstreamStatus::Missing)
        .map(|table| table.name)
        .collect()
}

/// Catalog drift as the stored probe describes it: no source connection, so
/// cheap enough for every supervisor cadence and every metrics scrape.
///
/// # Errors
///
/// Returns the metadata error. A database with no parseable probe has no
/// inventory to compare and reports no drift.
pub(crate) fn stored_drift(
    metadata: &pintail_meta::MetaStore,
    database: &DatabaseRecord,
) -> anyhow::Result<Vec<String>> {
    let Some(report) = database
        .probe_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<ProbeReport>(json).ok())
    else {
        return Ok(Vec::new());
    };
    let catalog = metadata.tables(&database.id)?;
    Ok(missing_from_catalog(
        &report.tables,
        &catalog,
        &decode_names(database.include_tables.as_deref()),
        &decode_names(database.exclude_tables.as_deref()),
    ))
}

/// `GET /api/databases/{id}/upstream-tables`: probes the source and lists
/// every base table with where it stands against the catalog.
///
/// Read-only: the stored probe is left as it was, so looking never changes
/// what replication does.
pub(crate) async fn list(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<UpstreamTablesResponse>, ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    let record = crate::databases::load_database(&state, &principal, &id)?;
    require_source(&record)?;
    let report = probe_source(&state, &record).await?;
    let catalog = state.metadata()?.tables(&id).map_err(ApiError::internal)?;
    let include = decode_names(record.include_tables.as_deref());
    let tables = classify(
        &report.tables,
        &catalog,
        &include,
        &decode_names(record.exclude_tables.as_deref()),
    );
    let mut counts = StatusCounts::default();
    for table in &tables {
        match table.status {
            UpstreamStatus::Mirrored => counts.mirrored += 1,
            UpstreamStatus::Missing => counts.missing += 1,
            UpstreamStatus::NotIncluded => counts.not_included += 1,
            UpstreamStatus::DroppedUpstream => counts.dropped_upstream += 1,
        }
    }
    Ok(Json(UpstreamTablesResponse {
        database_id: id,
        include_all: include.is_empty(),
        probed_at: Utc::now().to_rfc3339(),
        counts,
        tables,
    }))
}

/// `POST /api/databases/{id}/upstream-tables`: selects the named source
/// tables and starts the non-forced snapshot that copies them.
///
/// The snapshot on a live database copies exactly the selected tables the
/// catalog lacks, so this also repairs any other drift it finds on the way.
#[allow(clippy::too_many_lines)]
pub(crate) async fn add(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(request): Json<AddUpstreamTablesRequest>,
) -> Result<(StatusCode, Json<AddUpstreamTablesResponse>), ApiError> {
    principal.require_operator()?;
    principal.authorize_database(&id)?;
    let record = crate::databases::load_database(&state, &principal, &id)?;
    require_source(&record)?;
    let requested = request
        .tables
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if requested.is_empty() {
        return Err(ApiError::bad_request("name at least one table to add"));
    }
    // Refused before anything changes: a paused database would take the new
    // selection and then refuse the snapshot, leaving half an action behind.
    if record.mode == "paused" {
        return Err(ApiError::conflict(
            "resume the database before adding tables",
        ));
    }
    let report = probe_source(&state, &record).await?;
    let mut unknown = Vec::new();
    let mut names = Vec::new();
    for requested in requested {
        match report
            .tables
            .iter()
            .find(|source| source.name.eq_ignore_ascii_case(requested))
        {
            Some(source) => {
                if !names.contains(&source.name) {
                    names.push(source.name.clone());
                }
            }
            None => unknown.push(requested.to_owned()),
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError::bad_request(format!(
            "the source has no base table named {}",
            unknown.join(", ")
        )));
    }
    if record.keyless_policy == "reject" {
        let keyless = report
            .tables
            .iter()
            .filter(|source| {
                names.contains(&source.name)
                    && source.key.mode == pintail_types::KeyMode::AppendRowId
            })
            .map(|source| source.name.clone())
            .collect::<Vec<_>>();
        if !keyless.is_empty() {
            return Err(ApiError::conflict(format!(
                "keyless_policy reject: tables without a usable key: {}",
                keyless.join(", ")
            )));
        }
    }

    let mut include = decode_names(record.include_tables.as_deref());
    let mut exclude = decode_names(record.exclude_tables.as_deref());
    let mut added_to_include = Vec::new();
    // An empty include list already selects every table; appending to it
    // would narrow the selection to exactly these names.
    if !include.is_empty() {
        for name in &names {
            if !include
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(name))
            {
                include.push(name.clone());
                added_to_include.push(name.clone());
            }
        }
    }
    let mut removed_from_exclude = Vec::new();
    exclude.retain(|existing| {
        let chosen = names.iter().any(|name| name.eq_ignore_ascii_case(existing));
        if chosen {
            removed_from_exclude.push(existing.clone());
        }
        !chosen
    });

    let now = Utc::now().to_rfc3339();
    {
        let metadata = state.metadata()?;
        if !added_to_include.is_empty() || !removed_from_exclude.is_empty() {
            let includes = serde_json::to_string(&include).map_err(ApiError::internal)?;
            let excludes = serde_json::to_string(&exclude).map_err(ApiError::internal)?;
            metadata
                .update_database(
                    &id,
                    &DatabaseUpdate {
                        name: &record.name,
                        encrypted_dsn: None,
                        mode: &record.mode,
                        include_tables: Some(&includes),
                        exclude_tables: Some(&excludes),
                        poll_interval_seconds: record.poll_interval_seconds,
                        reconcile_interval_seconds: record.reconcile_interval_seconds,
                        keyless_policy: &record.keyless_policy,
                        now: &now,
                    },
                )
                .map_err(ApiError::internal)?;
        }
        // A snapshot on a live database copies from the STORED probe, which
        // predates any table created since it was taken. Probe JSON only:
        // the lifecycle state is not this request's to change.
        let encoded = serde_json::to_string(&report).map_err(ApiError::internal)?;
        metadata
            .refresh_database_probe_json(&id, &encoded, &now)
            .map_err(ApiError::internal)?;
    }
    let handed_off = state
        .metadata()?
        .snapshot_checkpoint(&id)
        .map_err(ApiError::internal)?
        .is_some();
    let (run_id, snapshot_state) =
        match crate::snapshot::begin_operator_snapshot(&state, &id, false).await {
            Ok(run_id) => (Some(run_id), "snapshotting"),
            // A live database's selection now names tables its catalog lacks,
            // which is exactly what the supervisor's catalog repair copies; it
            // is cleared to try on its next cadence rather than after its gap.
            Err(error) if handed_off && is_busy(&error) => {
                state.forget_catalog_repair(&id);
                (None, "queued")
            }
            Err(error) => return Err(error),
        };
    audit::record(
        &state,
        &principal,
        "database.upstream_tables.add",
        Some(("database", &id)),
        Some(serde_json::json!({
            "tables": names,
            "added_to_include": added_to_include,
            "removed_from_exclude": removed_from_exclude,
            "run_id": run_id,
        })),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(AddUpstreamTablesResponse {
            added_to_include,
            removed_from_exclude,
            run_id,
            state: snapshot_state,
        }),
    ))
}

/// Starts the automatic repair when the catalog lacks included tables the
/// stored probe lists. Called by the supervisor after each cycle has
/// released the job slot.
///
/// Only a database that has handed off to replication is repaired: before
/// that, the first snapshot is still the thing copying its tables. Tables
/// created mid-stream never reach here - the stream's DDL adoption copies
/// them and records them in the catalog and the stored probe together - so
/// what this finds is a catalog that lost rows the stream will never
/// recreate.
pub(crate) fn repair_catalog_drift(state: &ApiState, database_id: &str) {
    let Ok(metadata) = state.metadata() else {
        return;
    };
    let Ok(Some(database)) = metadata.database(database_id) else {
        return;
    };
    if database.kind == "local" || database.mode == "paused" {
        return;
    }
    if !matches!(metadata.snapshot_checkpoint(database_id), Ok(Some(_))) {
        return;
    }
    let missing = match stored_drift(&metadata, &database) {
        Ok(missing) => missing,
        Err(error) => {
            state.publish(ApiEvent::database(
                "catalog.drift.error",
                database_id,
                format!("could not compare the catalog against the probe: {error}"),
            ));
            return;
        }
    };
    drop(metadata);
    if missing.is_empty() || !state.claim_catalog_repair(database_id, CATALOG_REPAIR_GAP) {
        return;
    }
    state.publish(ApiEvent::database(
        "catalog.drift",
        database_id,
        format!(
            "warning: {} included table(s) exist on the source but not in the catalog, \
             so nothing mirrors them: {}",
            missing.len(),
            summarize_names(missing.iter().map(String::as_str))
        ),
    ));
    match crate::snapshot::begin_snapshot_job(state, database_id, false) {
        Ok(run_id) => state.publish(ApiEvent::database(
            "catalog.repair",
            database_id,
            format!(
                "copying the {} missing table(s) via snapshot {run_id}",
                missing.len()
            ),
        )),
        Err(error) => state.publish(ApiEvent::database(
            "catalog.repair_failed",
            database_id,
            format!(
                "the catalog repair could not start and retries in {}s: {error}",
                CATALOG_REPAIR_GAP.as_secs()
            ),
        )),
    }
}

fn is_busy(error: &ApiError) -> bool {
    error.status() == StatusCode::CONFLICT && error.to_string().contains("job slot")
}

fn require_source(record: &DatabaseRecord) -> Result<(), ApiError> {
    if record.kind == "local" {
        return Err(ApiError::conflict(
            "upstream tables need a replicated source; this is a local database",
        ));
    }
    Ok(())
}

async fn probe_source(state: &ApiState, record: &DatabaseRecord) -> Result<ProbeReport, ApiError> {
    let dsn = state.decrypt_dsn(&record.encrypted_dsn)?;
    let opts = crate::dsn::source_opts(&dsn)
        .map_err(|error| ApiError::bad_request(format!("invalid MySQL DSN: {error}")))?;
    let pool = Pool::new(opts);
    // Listing and adding need names and shapes; the counts shown are the
    // source's own estimates, never a `COUNT(*)` of every table.
    let report = probe_with(&pool, &record.name, RowCounts::Estimated).await;
    let _ = pool.disconnect().await;
    report.map_err(|error| ApiError::unavailable(format!("could not probe the source: {error}")))
}

fn decode_names(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default()
}

impl ApiState {
    /// Claims the right to start a catalog repair for a database, at most
    /// once per `gap`.
    pub(crate) fn claim_catalog_repair(&self, database_id: &str, gap: Duration) -> bool {
        self.with_catalog_repairs(|repairs| {
            let now = Instant::now();
            match repairs.get(database_id) {
                Some(last) if now.duration_since(*last) < gap => false,
                _ => {
                    repairs.insert(database_id.to_owned(), now);
                    true
                }
            }
        })
        .unwrap_or(false)
    }

    /// Lets the next supervisor cadence attempt a repair for the database.
    pub(crate) fn forget_catalog_repair(&self, database_id: &str) {
        let _ = self.with_catalog_repairs(|repairs| repairs.remove(database_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, rows: Option<u64>) -> SourceTable {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "engine": "InnoDB",
            "estimated_rows": rows,
            "rows_are_exact": false,
            "columns": [],
            "key": {"mode": "primary", "index_name": "PRIMARY", "columns": ["id"]},
            "unique_keys": [],
            "requires_reconciliation": false,
            "foreign_keys": [],
            "warnings": [],
        }))
        .expect("source table")
    }

    fn record(name: &str, state: &str) -> TableRecord {
        TableRecord {
            database_id: "db".to_owned(),
            name: name.to_owned(),
            state: state.to_owned(),
            primary_key_json: None,
            cursor_column: None,
            sort_key_json: None,
            rows_synced: 0,
            last_error: None,
            last_reconcile_at: None,
            schema_version: 1,
            orphaned_at: None,
            soft_delete_column: None,
            copy_complete: true,
            copy_pending: false,
            paused: None,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn statuses(tables: &[UpstreamTable]) -> Vec<(&str, UpstreamStatus)> {
        tables
            .iter()
            .map(|table| (table.name.as_str(), table.status))
            .collect()
    }

    #[test]
    fn an_included_table_the_catalog_lost_is_missing_not_mirrored() {
        let upstream = [source("alpha", Some(10)), source("beta", Some(20))];
        let catalog = [record("alpha", "streaming")];
        let tables = classify(&upstream, &catalog, &names(&["alpha", "beta"]), &[]);
        assert_eq!(
            statuses(&tables),
            vec![
                ("alpha", UpstreamStatus::Mirrored),
                ("beta", UpstreamStatus::Missing),
            ]
        );
        assert_eq!(tables[0].catalog_state.as_deref(), Some("streaming"));
        assert_eq!(tables[1].estimated_rows, Some(20));
        assert_eq!(tables[1].catalog_state, None);
    }

    #[test]
    fn tables_outside_the_selection_are_not_included() {
        let upstream = [
            source("alpha", None),
            source("beta", None),
            source("gamma", None),
        ];
        let tables = classify(&upstream, &[], &names(&["alpha"]), &names(&["gamma"]));
        assert_eq!(
            statuses(&tables),
            vec![
                ("alpha", UpstreamStatus::Missing),
                ("beta", UpstreamStatus::NotIncluded),
                ("gamma", UpstreamStatus::NotIncluded),
            ]
        );
        assert!(tables[2].excluded);
        assert!(!tables[1].excluded);
    }

    #[test]
    fn an_empty_include_list_selects_every_table_but_the_excluded() {
        let upstream = [source("alpha", None), source("beta", None)];
        let tables = classify(&upstream, &[], &[], &names(&["BETA"]));
        assert_eq!(
            statuses(&tables),
            vec![
                ("alpha", UpstreamStatus::Missing),
                ("beta", UpstreamStatus::NotIncluded),
            ]
        );
    }

    #[test]
    fn a_catalog_table_the_source_dropped_is_listed_as_dropped_upstream() {
        let upstream = [source("alpha", None)];
        let catalog = [record("alpha", "streaming"), record("retired", "streaming")];
        let tables = classify(&upstream, &catalog, &[], &[]);
        assert_eq!(
            statuses(&tables),
            vec![
                ("alpha", UpstreamStatus::Mirrored),
                ("retired", UpstreamStatus::DroppedUpstream),
            ]
        );
    }

    #[test]
    fn names_compare_without_case_everywhere() {
        let upstream = [source("Alpha", None), source("Beta", None)];
        let catalog = [record("alpha", "polling")];
        let tables = classify(&upstream, &catalog, &names(&["ALPHA", "beta"]), &[]);
        assert_eq!(
            statuses(&tables),
            vec![
                ("Alpha", UpstreamStatus::Mirrored),
                ("Beta", UpstreamStatus::Missing),
            ]
        );
    }

    #[test]
    fn an_excluded_table_still_in_the_catalog_reads_as_mirrored() {
        let upstream = [source("alpha", None)];
        let catalog = [record("alpha", "streaming")];
        let tables = classify(&upstream, &catalog, &[], &names(&["alpha"]));
        assert_eq!(statuses(&tables), vec![("alpha", UpstreamStatus::Mirrored)]);
        assert!(tables[0].excluded);
    }

    #[test]
    fn missing_from_catalog_names_only_drift() {
        let upstream = [
            source("alpha", None),
            source("beta", None),
            source("gamma", None),
        ];
        let catalog = [record("alpha", "streaming"), record("gone", "streaming")];
        assert_eq!(
            missing_from_catalog(&upstream, &catalog, &names(&["alpha", "beta"]), &[]),
            vec!["beta".to_owned()]
        );
    }

    #[test]
    fn statuses_serialize_in_kebab_case() {
        assert_eq!(
            serde_json::to_value([
                UpstreamStatus::Mirrored,
                UpstreamStatus::Missing,
                UpstreamStatus::NotIncluded,
                UpstreamStatus::DroppedUpstream,
            ])
            .expect("json"),
            serde_json::json!(["mirrored", "missing", "not-included", "dropped-upstream"])
        );
    }

    #[test]
    fn a_catalog_repair_is_claimed_at_most_once_per_gap() {
        let data = tempfile::tempdir().expect("data directory");
        let state = ApiState::new(
            data.path(),
            data.path().join("meta.db"),
            b"test-jwt-secret-with-enough-entropy".to_vec(),
            &"42".repeat(32),
        )
        .expect("state");
        let gap = Duration::from_secs(600);
        assert!(state.claim_catalog_repair("db-1", gap));
        assert!(!state.claim_catalog_repair("db-1", gap));
        assert!(state.claim_catalog_repair("db-2", gap));
        state.forget_catalog_repair("db-1");
        assert!(state.claim_catalog_repair("db-1", gap));
        assert!(state.claim_catalog_repair("db-3", Duration::ZERO));
        assert!(state.claim_catalog_repair("db-3", Duration::ZERO));
    }
}
