//! Controlled SQL ablations using replica-like catalog metadata.
use crate::{Data, anchor, live};
use pintail_catalog::{
    CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
};
use pintail_exec::{
    ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider,
    collation::Collation,
};
use pintail_sql::{Binder, parse_statement};
use pintail_store::TableSnapshot;
use pintail_types::Value;
pub const NAMES: [&str; 7] = [
    "ordinary",
    "projection-only",
    "filter-only",
    "filter-project",
    "factor-only",
    "factor-filter-project",
    "identity-derived",
];
pub fn sql(v: usize, d: &Data) -> String {
    let limit = d.domain.min(512);
    let query = match v {
        0 => anchor::sql(5, d),
        1 => format!(
            "SELECT d.g,COUNT(*),COUNT(f.v),SUM(f.v) FROM facts f JOIN (SELECT id,k,g FROM facts) d ON f.k=d.k WHERE d.id < {limit} AND MOD(d.id,7) <> 0 GROUP BY d.g ORDER BY d.g"
        ),
        2 => format!(
            "SELECT d.g,COUNT(*),COUNT(f.v),SUM(f.v) FROM facts f JOIN (SELECT * FROM facts WHERE id < {limit} AND MOD(id,7) <> 0) d ON f.k=d.k GROUP BY d.g ORDER BY d.g"
        ),
        3 => anchor::filtered_join_sql(d),
        4 => format!(
            "SELECT d.g,SUM(f.n),SUM(f.c),SUM(f.s) FROM (SELECT k,COUNT(*) AS n,COUNT(v) AS c,SUM(v) AS s FROM facts GROUP BY k) f JOIN facts d ON f.k=d.k WHERE d.id < {limit} AND MOD(d.id,7) <> 0 GROUP BY d.g ORDER BY d.g"
        ),
        5 => anchor::factorized_join_sql(d),
        6 => format!(
            "SELECT d.g,COUNT(*),COUNT(f.v),SUM(f.v) FROM facts f JOIN (SELECT * FROM facts) d ON f.k=d.k WHERE d.id < {limit} AND MOD(d.id,7) <> 0 GROUP BY d.g ORDER BY d.g"
        ),
        _ => unreachable!(),
    };
    if std::env::var_os("PINTAIL_PROOF_PK_JOIN").is_some() {
        query
            .replace("SELECT k,g FROM facts", "SELECT id,k,g FROM facts")
            .replace("f.k=d.k", "f.k=d.id")
    } else {
        query
    }
}
pub fn expected(d: &Data) -> Vec<Vec<String>> {
    if std::env::var_os("PINTAIL_PROOF_PK_JOIN").is_none() {
        return anchor::expected(5, d);
    }
    let dim: std::collections::BTreeMap<_, _> = d
        .rows
        .iter()
        .filter(|r| r.id < d.domain.min(512) && !r.id.is_multiple_of(7))
        .map(|r| (r.id, r.low))
        .collect();
    let mut groups = crate::Groups::new();
    for r in &d.rows {
        if let Some(g) = dim.get(&r.key) {
            groups.entry(*g).or_default().add(r);
        }
    }
    groups
        .into_iter()
        .map(|(k, a)| {
            vec![
                k.to_string(),
                a.rows.to_string(),
                a.count.to_string(),
                if a.count == 0 {
                    "NULL".into()
                } else {
                    a.sum.to_string()
                },
            ]
        })
        .collect()
}
pub fn execute(
    snapshot: &TableSnapshot,
    d: &Data,
    sql: &str,
    metadata: usize,
    cap: usize,
    keep_plan: bool,
) -> (Result<Vec<Vec<String>>, ExecError>, String, String) {
    let db = DatabaseId::new(1);
    let id = TableId::new(1);
    let estimate = std::env::var("PINTAIL_PROOF_ESTIMATE")
        .ok()
        .map_or(d.rows.len() as u64, |s| s.parse().unwrap());
    let stats = if [1, 3].contains(&metadata) {
        TableStatistics::with_estimated_row_count(estimate)
    } else {
        TableStatistics::with_row_count(d.rows.len() as u64)
    };
    let mut entry = TableEntry::new(id, "facts", live::schema(), stats).unwrap();
    if [1, 2].contains(&metadata) {
        entry = entry.with_key_columns([1]).unwrap();
    }
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(db, "lab", [entry]).unwrap()]).unwrap();
    let provider = SnapshotScanProvider::new([(db, id, snapshot)]).unwrap();
    let bound = Binder::new(&catalog, Some("lab"))
        .bind(&parse_statement(sql).unwrap())
        .unwrap();
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .unwrap();
    let physical = if keep_plan {
        format!("{plan:?}")
    } else {
        String::new()
    };
    let mut exec = match Execution::start_profiled(plan, &provider, cap, None, Collation::default())
    {
        Ok(e) => e,
        Err(e) => return (Err(e), physical, String::new()),
    };
    let mut out = Vec::new();
    let result = loop {
        match exec.next_batch() {
            Ok(Some(batch)) => {
                for i in batch.selection().selected_rows() {
                    out.push(
                        batch
                            .columns()
                            .iter()
                            .map(|c| match c.value(i).unwrap() {
                                Value::Null => "NULL".into(),
                                Value::Int64(v) => v.to_string(),
                                Value::UInt64(v) => v.to_string(),
                                Value::Utf8(v) => {
                                    if let Some((whole, fraction)) = v.split_once('.') {
                                        assert!(fraction.chars().all(|c| c == '0'));
                                        whole.into()
                                    } else {
                                        v.clone()
                                    }
                                }
                                v => panic!("unexpected value {v:?}"),
                            })
                            .collect(),
                    );
                }
            }
            Ok(None) => break Ok(out),
            Err(e) => break Err(e),
        }
    };
    (
        result,
        physical,
        exec.profile().map_or_else(String::new, |p| p.render()),
    )
}
