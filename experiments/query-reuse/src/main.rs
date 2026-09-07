use std::{sync::{Arc, Barrier, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::Instant};
use pintail_catalog::{CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics};
use pintail_exec::{Execution, LogicalPlanner, Optimizer, PhysicalPlanner, SnapshotScanProvider, collation::Collation};
use pintail_sql::{Binder, BoundQuery, parse_statement};
use pintail_store::{TableStore, TableSnapshot, StoreOptions};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use query_reuse_lab::{Answer, Epochs, Flights, RequestKey, Reuse, Rows, dependencies};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
const IDS: [u32; 4] = [1,2,3,4];
const SQL: &str = "SELECT bucket, SUM(metric), COUNT(*) FROM samples WHERE metric >= 0 AND id % 7 <> 0 GROUP BY bucket ORDER BY bucket";
fn schema() -> TableSchema {
    TableSchema::new(1, vec![Column::new(1,"id",DataType::UInt64,false), Column::new(2,"bucket",DataType::UInt64,false), Column::new(3,"metric",DataType::Int64,true), Column::new(4,"note",DataType::Utf8,true)]).unwrap()
}
fn make_row(id: usize, version: u64, metric: i64, note: &str, deleted: bool) -> StoredRow {
    StoredRow::new(PrimaryKey::new(vec![KeyPart::UInt64(id as u64)]).unwrap(),
        vec![Value::UInt64(id as u64),Value::UInt64(id as u64 % 64),Value::Int64(metric),Value::Utf8(note.into())],version,deleted)
}
struct Fixture { _dir: tempfile::TempDir, table: TableStore, catalog: CatalogSnapshot, rows: Vec<StoredRow>, epochs: Epochs }
impl Fixture {
    fn new(n: usize) -> Self {
        let dir=tempfile::tempdir().unwrap();
        let mut table=TableStore::open(dir.path(),schema(),StoreOptions { background_compaction: false, ..Default::default() }).unwrap();
        let rows: Vec<_>=(0..n).map(|i| make_row(i,i as u64+1,(i % 1000) as i64,"initial",false)).collect();
        table.bulk_ingest_snapshot(rows.clone()).unwrap();
        let entry=TableEntry::new(TableId::new(1),"samples",schema(),TableStatistics::with_row_count(n as u64)).unwrap();
        let catalog=CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1),"lab",[entry]).unwrap()]).unwrap();
        Self { _dir:dir,table,catalog,rows,epochs:Epochs::default() }
    }
    #[cfg(test)]
    fn publish(&mut self, index: usize, next: StoredRow, known: bool) {
        self.table.ingest_cdc(vec![next.clone()]).unwrap();
        if known { self.epochs.change(&IDS,self.rows.get(index),&next); } else { self.epochs.unknown(); }
        if index < self.rows.len() { self.rows[index]=next; } else { self.rows.push(next); }
    }
    fn bind(&self, sql:&str)->BoundQuery { Binder::new(&self.catalog,Some("lab")).bind(&parse_statement(sql).unwrap()).unwrap() }
}
fn query(catalog:&CatalogSnapshot,snapshot:&TableSnapshot,sql:&str)->Answer {
    let statement=parse_statement(sql).map_err(|e|e.to_string())?;
    let bound=Binder::new(catalog,Some("lab")).bind(&statement).map_err(|e|e.to_string())?;
    let physical=PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(bound)),Collation::default()).map_err(|e|e.to_string())?;
    let provider=SnapshotScanProvider::new([(DatabaseId::new(1),TableId::new(1),snapshot)]).map_err(|e|e.to_string())?;
    let mut execution=Execution::start(physical,&provider,64*1024*1024,Collation::default()).map_err(|e|e.to_string())?;
    let mut rows=Vec::new();
    while let Some(batch)=execution.next_batch().map_err(|e|e.to_string())? {
        for row in batch.selection().selected_rows() {
            rows.push(batch.columns().iter().map(|c|c.value(row).unwrap().clone()).collect());
            if rows.len()>10000 { return Err("lab output ceiling".into()); }
        }
    }
    Ok(Arc::new(rows))
}
fn encoded(rows:&Rows)->Vec<u8> { serde_json::to_vec(rows).unwrap() }
fn cpu_ticks()->u64 {
 let s=std::fs::read_to_string("/proc/self/stat").unwrap();let fields:Vec<_>=s.rsplit_once(") ").unwrap().1.split_whitespace().collect();fields[11].parse::<u64>().unwrap()+fields[12].parse::<u64>().unwrap()
}
fn peak_rss()->u64 {
    std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines().find_map(|s|s.strip_prefix("VmHWM:").and_then(|s|s.split_whitespace().next()).and_then(|s|s.parse().ok())).unwrap_or(0)
}
struct Finished<'a> { count: &'a AtomicUsize, stop: &'a AtomicBool, clients: usize }
impl Drop for Finished<'_> { fn drop(&mut self) { if self.count.fetch_add(1,Ordering::Relaxed)+1==self.clients {self.stop.store(true,Ordering::Relaxed);} } }
fn flights(n:usize,clients:usize,duplicates:usize,shared:bool) {
    let fixture=Fixture::new(n); let snapshot=fixture.table.snapshot();
    let coordinator=Flights::default(); let count=AtomicUsize::new(0);
    let barrier=Barrier::new(clients); let latencies=Mutex::new(Vec::new());
    let queries:Vec<_>=(0..clients).map(|i| if i<duplicates { SQL.to_owned() } else { format!("SELECT bucket, SUM(metric), COUNT(*) FROM samples WHERE metric >= {} AND id % 7 <> 0 GROUP BY bucket ORDER BY bucket",i+1) }).collect();
    let expected:Vec<_>=queries.iter().map(|q| encoded(&query(&fixture.catalog,&snapshot,q).unwrap())).collect();
    let stop=AtomicBool::new(false);let peak=AtomicUsize::new(0);let finished=AtomicUsize::new(0);
    let cpu_before=cpu_ticks(); let start=Instant::now();
    std::thread::scope(|scope| {
        let stop_ref=&stop;let peak_ref=&peak;
        scope.spawn(move|| { while !stop_ref.load(Ordering::Relaxed) {peak_ref.fetch_max(pintail_exec::shared_memory_budget().used(),Ordering::Relaxed);std::thread::sleep(std::time::Duration::from_micros(500));} });
        let finished=&finished;
        for (sql,expected) in queries.iter().zip(&expected) {
            let snapshot=&snapshot; let catalog=&fixture.catalog; let barrier=&barrier; let coordinator=&coordinator; let count=&count; let latencies=&latencies;
            scope.spawn(move || {
                let _finished=Finished {count:finished,stop:stop_ref,clients};barrier.wait(); let start=Instant::now();
                let compute=|| { count.fetch_add(1,Ordering::Relaxed); query(catalog,snapshot,sql) };
                let answer=if shared { coordinator.run(RequestKey { snapshot:0,sql:sql.clone(),scope:0,settings:0,max_rows:10000 },&AtomicBool::new(false),compute) } else {compute()}.unwrap();
                let wire=encoded(&answer);
                latencies.lock().unwrap().push(start.elapsed().as_nanos() as u64);
                assert_eq!(&wire,expected);
            });
        }
    });
    let elapsed=start.elapsed().as_nanos() as u64;
    assert_eq!(coordinator.active(),0);
    println!("{}",serde_json::json!({"experiment":"flights","shared":shared,"rows":n,"clients":clients,"duplicates":duplicates,"executions":count.load(Ordering::Relaxed),"followers":coordinator.followers.load(Ordering::Relaxed),"elapsed_ns":elapsed,"cpu_ticks":cpu_ticks()-cpu_before,"sampled_query_bytes":peak.load(Ordering::Relaxed),"latency_ns":latencies.into_inner().unwrap(),"process_peak_rss_kib":peak_rss()}));
}
fn epochs(n:usize,ratio:usize,cached:bool) {
    let mut fixture=Fixture::new(n); let deps=dependencies(&fixture.bind(SQL)).unwrap(); assert_eq!(deps.into_iter().collect::<Vec<_>>(),vec![1,2,3]);
    let mut reuse=Reuse::default(); let mut hits=0; let mut executions=0; let mut query_ns=0u64; let mut tracking_ns=0u64; let mut ingest_ns=0u64; let mut expected_time=0u64; let mut relevant_updates=0;
    let mut latency=Vec::new();
    for step in 0..41 {
        if step>0 {
            let id=step*13; if (step*37)%100<ratio { relevant_updates+=1; } let next=make_row(id,n as u64+step as u64+1,if (step*37)%100<ratio {(id%1000) as i64+step as i64} else {(id%1000) as i64},&format!("update-{step}"),false);
            let start=Instant::now(); fixture.table.ingest_cdc(vec![next.clone()]).unwrap(); ingest_ns+=start.elapsed().as_nanos() as u64;
            let start=Instant::now(); if cached { fixture.epochs.change(&IDS,fixture.rows.get(id),&next); } tracking_ns+=start.elapsed().as_nanos() as u64; fixture.rows[id]=next;
        }
        let snapshot=fixture.table.snapshot();
        let start=Instant::now();
        let token=if cached { let bound=fixture.bind(SQL);let deps=dependencies(&bound).unwrap();Some(fixture.epochs.token(&deps)) } else {None};
        let answer=if let Some(rows)=token.as_ref().and_then(|token|reuse.get(SQL,token)) { hits+=1;rows } else { executions+=1;let rows=query(&fixture.catalog,&snapshot,SQL).unwrap();if cached {reuse.put(SQL.into(),token.unwrap(),Arc::clone(&rows));} rows };
        let bytes=encoded(&answer); let elapsed=start.elapsed().as_nanos() as u64; query_ns+=elapsed; latency.push(elapsed);
        let start=Instant::now(); assert_eq!(bytes,encoded(&query(&fixture.catalog,&snapshot,SQL).unwrap())); expected_time+=start.elapsed().as_nanos() as u64;
    }
    println!("{}",serde_json::json!({"experiment":"epochs","cached":cached,"rows":n,"relevant_percent":ratio,"actual_relevant_updates":relevant_updates,"hits":hits,"executions":executions,"query_ns":query_ns,"tracking_ns":tracking_ns,"ingest_ns":ingest_ns,"verification_ns":expected_time,"latency_ns":latency,"process_peak_rss_kib":peak_rss()}));
}
fn main() {
    let args:Vec<_>=std::env::args().collect();
    match args[1].as_str() {
        "flights"=>flights(args[2].parse().unwrap(),args[3].parse().unwrap(),args[4].parse().unwrap(),args[5]=="shared"),
        "epochs"=>epochs(args[2].parse().unwrap(),args[3].parse().unwrap(),args[4]=="cached"),
        _=>panic!("unknown experiment"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binder_tracks_filter_and_hidden_order_columns_and_refuses_unknown_shapes() {
        let f=Fixture::new(64);
        for (sql,ids) in [("SELECT note FROM samples WHERE metric > 3 ORDER BY bucket",vec![2,3,4]), ("SELECT COUNT(*) FROM samples",vec![]), (SQL,vec![1,2,3])] {
            assert_eq!(dependencies(&f.bind(sql)).unwrap().into_iter().collect::<Vec<_>>(),ids);
        }
        for sql in ["SELECT RAND() FROM samples", "SELECT NOW() FROM samples", "SELECT a.id FROM samples a JOIN samples b ON a.id=b.id", "SELECT metric FROM samples UNION ALL SELECT metric FROM samples"] {
            assert!(dependencies(&f.bind(sql)).is_none(),"{sql}");
        }
    }
    #[test]
    fn actual_store_updates_preserve_only_proven_dependencies() {
        let mut f=Fixture::new(64); let deps=dependencies(&f.bind(SQL)).unwrap(); let mut cache=Reuse::default();
        let old_snapshot=f.table.snapshot(); let old=query(&f.catalog,&old_snapshot,SQL).unwrap(); let old_token=f.epochs.token(&deps);cache.put(SQL.into(),old_token.clone(),Arc::clone(&old));
        f.publish(13,make_row(13,100,13,"other",false),true);
        assert_eq!(f.epochs.token(&deps),old_token);assert_eq!(*cache.get(SQL,&old_token).unwrap(),*query(&f.catalog,&f.table.snapshot(),SQL).unwrap());
        f.publish(13,make_row(13,101,-1,"other",false),true);
        assert!(cache.get(SQL,&f.epochs.token(&deps)).is_none());assert_ne!(*old,*query(&f.catalog,&f.table.snapshot(),SQL).unwrap());
        assert_eq!(*old,*query(&f.catalog,&old_snapshot,SQL).unwrap());
        let before=f.epochs.token(&deps);f.publish(64,make_row(64,102,400,"insert",false),true);assert_ne!(before,f.epochs.token(&deps));
        let before=f.epochs.token(&deps);f.publish(12,make_row(12,103,12,"delete",true),true);assert_ne!(before,f.epochs.token(&deps));
        let before=f.epochs.token(&deps);f.publish(14,make_row(14,104,14,"unknown",false),false);assert_ne!(before,f.epochs.token(&deps));
        f.table.flush().unwrap();let after_flush=query(&f.catalog,&f.table.snapshot(),SQL).unwrap();drop(f.table);
        let reopened=TableStore::open(f._dir.path(),schema(),StoreOptions::default()).unwrap();assert_eq!(*after_flush,*query(&f.catalog,&reopened.snapshot(),SQL).unwrap());
    }
    #[test]
    fn old_completion_cannot_validate_new_snapshot_and_schema_changes_invalidate() {
        let mut f=Fixture::new(64);let deps=dependencies(&f.bind(SQL)).unwrap();
        let old_snapshot=f.table.snapshot();let old_token=f.epochs.token(&deps);let catalog=f.catalog.clone();
        let (go_tx,go_rx)=std::sync::mpsc::channel();
        let old=std::thread::spawn(move|| { go_rx.recv().unwrap();query(&catalog,&old_snapshot,SQL).unwrap() });
        f.publish(13,make_row(13,100,-1,"relevant",false),true);
        let new_token=f.epochs.token(&deps);go_tx.send(()).unwrap();
        let mut cache=Reuse::default();cache.put(SQL.into(),old_token,old.join().unwrap());assert!(cache.get(SQL,&new_token).is_none());
        let before=query(&f.catalog,&f.table.snapshot(),SQL).unwrap();
        let new_schema=TableSchema::new(2,vec![Column::new(1,"id",DataType::UInt64,false),Column::new(2,"bucket",DataType::UInt64,false),Column::new(3,"metric",DataType::Int64,true),Column::new(4,"annotation",DataType::Utf8,true)]).unwrap();
        f.table.evolve_schema(new_schema.clone()).unwrap();f.epochs.unknown();
        assert!(query(&f.catalog,&f.table.snapshot(),SQL).is_err());
        let entry=TableEntry::new(TableId::new(1),"samples",new_schema,TableStatistics::with_row_count(64)).unwrap();f.catalog=CatalogSnapshot::new([DatabaseEntry::new(DatabaseId::new(1),"lab",[entry]).unwrap()]).unwrap();assert_ne!(new_token,f.epochs.token(&deps));
        assert_eq!(*before,*query(&f.catalog,&f.table.snapshot(),SQL).unwrap());
    }

}
