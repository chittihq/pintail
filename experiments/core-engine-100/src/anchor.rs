//! Parse/bind/plan/execute anchors on the exact same changing table.
use crate::{Data, live, run};
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
pub fn sql(case: usize, d: &Data) -> String {
    match case{
1=>"SELECT id,v FROM facts WHERE g < 4 AND v >= 8000 ORDER BY id".into(),
2=>"SELECT id,k,g,v FROM facts ORDER BY id".into(),
3=>"SELECT g,COUNT(*),COUNT(v),SUM(v) FROM facts GROUP BY g ORDER BY g".into(),
4=>"SELECT k,COUNT(*),COUNT(v),SUM(v) AS s FROM facts GROUP BY k ORDER BY s DESC,k LIMIT 32".into(),
5=>format!("SELECT d.g,COUNT(*),COUNT(f.v),SUM(f.v) FROM facts f JOIN facts d ON f.k=d.k WHERE d.id < {} AND MOD(d.id,7) <> 0 GROUP BY d.g ORDER BY d.g",d.domain.min(512)),
6=>"SELECT g,COUNT(DISTINCT v) FROM facts GROUP BY g ORDER BY g".into(),
7=>"SELECT id,v FROM facts ORDER BY v DESC,id LIMIT 32".into(),
8=>{let w=if d.scenario==1{255}else{63};format!("SELECT id,SUM(v) OVER (ORDER BY id ROWS {w} PRECEDING),COUNT(v) OVER (ORDER BY id ROWS {w} PRECEDING),MIN(v) OVER (ORDER BY id ROWS {w} PRECEDING) FROM facts ORDER BY id")},
9=>"SELECT id,(CASE WHEN v IS NULL THEN NULL ELSE k END) IN (SELECT CASE WHEN v IS NULL THEN NULL ELSE k END FROM facts WHERE MOD(id,3)=0), (CASE WHEN v IS NULL THEN NULL ELSE k END) NOT IN (SELECT CASE WHEN v IS NULL THEN NULL ELSE k END FROM facts WHERE MOD(id,3)=0) FROM facts ORDER BY id".into(),
10=>"SELECT o.id,o.k,(SELECT COUNT(*) FROM facts i WHERE i.k=o.k),(SELECT COUNT(v) FROM facts i WHERE i.k=o.k),(SELECT SUM(v) FROM facts i WHERE i.k=o.k) FROM facts o ORDER BY o.id LIMIT 128".into(),_=>unreachable!()}
}
pub fn filtered_join_sql(d: &Data) -> String {
    format!(
        "SELECT d.g,COUNT(*),COUNT(f.v),SUM(f.v) FROM facts f JOIN (SELECT k,g FROM facts WHERE id < {} AND MOD(id,7) <> 0) d ON f.k=d.k GROUP BY d.g ORDER BY d.g",
        d.domain.min(512)
    )
}
pub fn expected(case: usize, d: &Data) -> Vec<Vec<String>> {
    let null = "NULL".to_string();
    if case == 2 {
        return d
            .rows
            .iter()
            .map(|r| {
                vec![
                    r.id.to_string(),
                    r.key.to_string(),
                    r.low.to_string(),
                    if r.valid {
                        r.value.to_string()
                    } else {
                        null.clone()
                    },
                ]
            })
            .collect();
    }
    let values = run(case, 0, d);
    let width = match case {
        1 | 6 | 9 => 2,
        3 | 4 | 5 | 10 => 4,
        7 | 8 => 3,
        _ => unreachable!(),
    };
    values
        .chunks(width)
        .enumerate()
        .map(|(index, r)| {
            let mut row = match case {
                3 | 4 | 5 | 10 => vec![
                    r[0].to_string(),
                    r[1].to_string(),
                    r[2].to_string(),
                    if r[2] == 0 {
                        null.clone()
                    } else {
                        r[3].to_string()
                    },
                ],
                7 => vec![
                    r[0].to_string(),
                    if r[2] == 0 {
                        null.clone()
                    } else {
                        r[1].to_string()
                    },
                ],
                8 => vec![
                    if r[1] == 0 {
                        null.clone()
                    } else {
                        r[0].to_string()
                    },
                    r[1].to_string(),
                    if r[1] == 0 {
                        null.clone()
                    } else {
                        r[2].to_string()
                    },
                ],
                9 => r
                    .iter()
                    .map(|x| if *x == 2 { null.clone() } else { x.to_string() })
                    .collect(),
                _ => r.iter().map(ToString::to_string).collect(),
            };
            if [8, 9, 10].contains(&case) {
                row.insert(0, d.rows[index].id.to_string());
            }
            row
        })
        .collect()
}
fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Boolean(x) => u8::from(*x).to_string(),
        Value::Int64(x) => x.to_string(),
        Value::UInt64(x) => x.to_string(),
        Value::Utf8(s) => {
            if let Some((whole, fraction)) = s.split_once('.') {
                assert!(
                    fraction.chars().all(|c| c == '0'),
                    "non-integral answer {s}"
                );
                whole.to_string()
            } else {
                s.clone()
            }
        }
        other => panic!("unexpected result type {other:?}"),
    }
}
pub fn execute(snapshot: &TableSnapshot, d: &Data, sql: &str) -> Vec<Vec<String>> {
    try_execute(snapshot, d, sql).unwrap()
}
pub fn try_execute(
    snapshot: &TableSnapshot,
    d: &Data,
    sql: &str,
) -> Result<Vec<Vec<String>>, ExecError> {
    let db = DatabaseId::new(1);
    let table = TableId::new(1);
    let catalog = CatalogSnapshot::new([DatabaseEntry::new(
        db,
        "lab",
        [TableEntry::new(
            table,
            "facts",
            live::schema(),
            TableStatistics::with_row_count(d.rows.len() as u64),
        )
        .unwrap()],
    )
    .unwrap()])
    .unwrap();
    let provider = SnapshotScanProvider::new([(db, table, snapshot)]).unwrap();
    let statement = parse_statement(sql).unwrap();
    let bound = Binder::new(&catalog, Some("lab")).bind(&statement).unwrap();
    let plan = PhysicalPlanner::plan(
        Optimizer::optimize(LogicalPlanner::plan(bound)),
        Collation::default(),
    )
    .unwrap();
    let mut exec = Execution::start(plan, &provider, 256 << 20, Collation::default())?;
    let mut out = Vec::new();
    while let Some(batch) = exec.next_batch()? {
        for i in batch.selection().selected_rows() {
            out.push(
                batch
                    .columns()
                    .iter()
                    .map(|c| render(c.value(i).unwrap()))
                    .collect(),
            );
        }
    }
    Ok(out)
}
