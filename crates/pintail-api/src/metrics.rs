use std::{fmt::Write as _, path::Path, process::Command};

use axum::{
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::Response,
};
use chrono::{DateTime, Utc};

use crate::ApiState;

pub(crate) async fn metrics(State(state): State<ApiState>) -> Response {
    match render(&state) {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )
            .body(Body::from(body))
            .expect("metrics response is valid"),
        Err(error) => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(format!("metrics unavailable: {error}\n")))
            .expect("metrics error response is valid"),
    }
}

#[allow(clippy::too_many_lines)]
fn render(state: &ApiState) -> anyhow::Result<String> {
    let runtime = state.runtime_metrics();
    let metadata = state
        .metadata()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let databases = metadata.databases()?;
    let mut output = String::with_capacity(8 * 1024);
    counter(
        &mut output,
        "pintail_queries_total",
        "Read-only queries completed.",
        runtime.queries,
    );
    counter(
        &mut output,
        "pintail_query_rows_total",
        "Rows returned by read-only queries.",
        runtime.query_rows,
    );
    counter(
        &mut output,
        "pintail_query_duration_milliseconds_total",
        "Cumulative read-only query latency.",
        runtime.query_duration_ms,
    );
    counter(
        &mut output,
        "pintail_replication_cycles_total",
        "Finite supervised replication cycles attempted.",
        runtime.replication_cycles,
    );
    counter(
        &mut output,
        "pintail_replication_errors_total",
        "Supervised replication cycles that failed.",
        runtime.replication_errors,
    );
    counter(
        &mut output,
        "pintail_ingested_rows_total",
        "Rows and tombstones applied by supervised replication cycles.",
        runtime.ingested_rows,
    );
    gauge(
        &mut output,
        "pintail_process_resident_memory_bytes",
        "Resident memory reported by the host process table.",
        process_resident_bytes(),
    );

    output.push_str(
        "# HELP pintail_database_state Database state as a one-hot labeled gauge.\n\
         # TYPE pintail_database_state gauge\n\
         # HELP pintail_replication_lag_seconds Seconds since the database state last advanced.\n\
         # TYPE pintail_replication_lag_seconds gauge\n\
         # HELP pintail_table_rows Durable mirrored row counter.\n\
         # TYPE pintail_table_rows gauge\n\
         # HELP pintail_dead_letters Dead-letter records awaiting operator action.\n\
         # TYPE pintail_dead_letters gauge\n\
         # HELP pintail_backup_runs Backup runs by terminal or active status.\n\
         # TYPE pintail_backup_runs gauge\n",
    );
    let now = Utc::now();
    for database in &databases {
        let database_id = label(&database.id);
        let database_state = label(&database.state);
        let _ = writeln!(
            output,
            "pintail_database_state{{database=\"{database_id}\",state=\"{database_state}\"}} 1"
        );
        let lag = DateTime::parse_from_rfc3339(&database.updated_at)
            .ok()
            .and_then(|updated| {
                now.signed_duration_since(updated.with_timezone(&Utc))
                    .num_seconds()
                    .try_into()
                    .ok()
            })
            .unwrap_or(0_u64);
        let _ = writeln!(
            output,
            "pintail_replication_lag_seconds{{database=\"{database_id}\"}} {lag}"
        );
        for table in metadata.tables(&database.id)? {
            let _ = writeln!(
                output,
                "pintail_table_rows{{database=\"{database_id}\",table=\"{}\"}} {}",
                label(&table.name),
                table.rows_synced
            );
        }
        let dlq = metadata.dlq_records(Some(&database.id), 1_000_000)?.len();
        let _ = writeln!(
            output,
            "pintail_dead_letters{{database=\"{database_id}\"}} {dlq}"
        );
        let backups = metadata.backups(&database.id, 1_000_000)?;
        for status in ["running", "completed", "error"] {
            let count = backups
                .iter()
                .filter(|backup| backup.status == status)
                .count();
            let _ = writeln!(
                output,
                "pintail_backup_runs{{database=\"{database_id}\",status=\"{status}\"}} {count}"
            );
        }
    }

    let storage = storage_observation(
        state
            .data_dir()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?,
    );
    gauge(
        &mut output,
        "pintail_storage_bytes",
        "Bytes occupied by replica storage files.",
        storage.bytes,
    );
    gauge(
        &mut output,
        "pintail_storage_segments",
        "Immutable replica segment files.",
        storage.segments,
    );
    gauge(
        &mut output,
        "pintail_compaction_debt_segments",
        "Immutable segments above the default eight-segment steady-state target.",
        storage.segments.saturating_sub(8),
    );
    Ok(output)
}

#[derive(Default)]
struct StorageObservation {
    bytes: u64,
    segments: u64,
}

fn storage_observation(root: &Path) -> StorageObservation {
    let mut observation = StorageObservation::default();
    visit_storage(root, &mut observation);
    observation
}

fn visit_storage(path: &Path, observation: &mut StorageObservation) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            visit_storage(&path, observation);
        } else if metadata.is_file() {
            observation.bytes = observation.bytes.saturating_add(metadata.len());
            if path.extension().is_some_and(|extension| extension == "pts") {
                observation.segments = observation.segments.saturating_add(1);
            }
        }
    }
}

fn process_resident_bytes() -> u64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|rss| rss.trim().parse::<u64>().ok())
        .and_then(|kilobytes| kilobytes.checked_mul(1024))
        .unwrap_or(0)
}

fn counter(output: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        output,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
    );
}

fn gauge(output: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        output,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    );
}

fn label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
