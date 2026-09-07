use core_engine_100::{live::{Fixture,advance,read},names,run};
use std::{sync::{Arc,Barrier},time::Instant};
fn main(){
 let args:Vec<_>=std::env::args().collect();let arg=|i:usize,default:usize|args.get(i).map_or(default,|s|s.parse().unwrap());let(case,variant,n,scenario,seed)=(arg(1,1),arg(2,0),arg(3,20000),arg(4,0),arg(5,41));rayon::ThreadPoolBuilder::new().num_threads(4).build_global().unwrap();
 let mut fixture=Fixture::new(n,seed as u64,scenario);let mut readings=Vec::new();
 let phases=["settled","sparse-memtable","sparse-flushed-overlap","dense-hot-memtable","dense-flushed-overlap","mixed-overlap","compacted","stale-replay"];
 for(phase,label)in phases.into_iter().enumerate(){
  let mut expected=fixture.expected();if case!=2{expected.history=None;}let snapshot=fixture.table.snapshot();let expected_answer=run(case,0,&expected);let segments=fixture.table.compaction_status().unwrap().segment_count();let events=fixture.events(phase+1);let mutations=events.len();let barrier=Arc::new(Barrier::new(2));let writer_barrier=barrier.clone();let clock=Instant::now();
  let (query_ms,scan_ms,operator_ms,writer_start_ms,writer_ms,new_segments,compacted,actual)=std::thread::scope(|scope|{
   let table=&mut fixture.table;let writer=scope.spawn(||{writer_barrier.wait();let start=clock.elapsed().as_secs_f64()*1000.;let timer=Instant::now();let(s,c)=advance(table,&events,phase+1);(start,timer.elapsed().as_secs_f64()*1000.,s,c)});
   barrier.wait();let timer=Instant::now();let scanned=read(&snapshot,&expected);let scan_ms=timer.elapsed().as_secs_f64()*1000.;let op=Instant::now();let actual=run(case,variant,&scanned);let operator_ms=op.elapsed().as_secs_f64()*1000.;let query_ms=timer.elapsed().as_secs_f64()*1000.;let(ws,wm,ns,c)=writer.join().unwrap();(query_ms,scan_ms,operator_ms,ws,wm,ns,c,actual)
  });let cycle_ms=clock.elapsed().as_secs_f64()*1000.;
  assert_eq!(actual,expected_answer,"case={case} variant={variant} phase={label}");assert_eq!(read(&snapshot,&expected).rows,expected.rows,"old pinned snapshot");fixture.update_model(&events);let next=fixture.expected();assert_eq!(read(&fixture.table.snapshot(),&next).rows,next.rows,"latest committed state");
  readings.push(serde_json::json!({"phase":label,"query_ms":query_ms,"scan_ms":scan_ms,"operator_ms":operator_ms,"writer_start_ms":writer_start_ms,"writer_ms":writer_ms,"cycle_ms":cycle_ms,"mutations":mutations,"query_segments":segments,"next_segments":new_segments,"compacted_inputs":compacted,"snapshot_rows":expected.rows.len(),"correct":true}));
 }
 drop(fixture.table);let reopened=pintail_store::TableStore::open(fixture.directory.path(),core_engine_100::live::schema(),pintail_store::StoreOptions::default()).unwrap();let expected=core_engine_100::Data{rows:fixture.model.values().filter(|v|!v.dead).map(|v|v.row).collect(),domain:fixture.template.domain,low_domain:fixture.template.low_domain,scenario,history:None};assert_eq!(read(&reopened.snapshot(),&expected).rows,expected.rows,"restart recovery");
 let status=std::fs::read_to_string("/proc/self/status").unwrap_or_default();println!("{}",serde_json::json!({"case":case,"variant":variant,"name":names(case)[variant],"rows":n,"scenario":scenario,"seed":seed,"phases":readings,"process_peak":status.lines().find(|s|s.starts_with("VmHWM:")).unwrap_or("unavailable"),"correct":true,"restart_correct":true}));
}
