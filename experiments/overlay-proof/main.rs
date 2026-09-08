use pintail_store::{ProjectedScanStream, StoreOptions, TableStore};
use pintail_types::{Column, DataType, KeyPart, PrimaryKey, StoredRow, TableSchema, Value};
use std::time::Instant;
fn key(id: usize, text: bool) -> PrimaryKey {
    PrimaryKey::new(vec![if text { KeyPart::Utf8(format!("k{id:012}")) } else { KeyPart::UInt64(id as u64) }]).unwrap()
}
fn row(id: usize, text: bool, changed: bool) -> StoredRow {
    StoredRow::new(key(id,text), vec![if text {Value::Utf8(format!("k{id:012}"))} else {Value::UInt64(id as u64)}, Value::UInt64(id as u64),Value::UInt64(if changed {7} else {3}),Value::UInt64(id as u64+11)],if changed {2} else {1},changed && id%19==0)
}
fn main(){
 let n=100_000;
 for text in [false,true] {
  for changed in [0,1,1000,20_000] {
   let directory=tempfile::tempdir().unwrap();
   let schema=TableSchema::new(1,vec![Column::new(1,"id",if text {DataType::Utf8}else{DataType::UInt64},false),Column::new(2,"a",DataType::UInt64,false),Column::new(3,"b",DataType::UInt64,false),Column::new(4,"c",DataType::UInt64,false)]).unwrap();
   let mut table=TableStore::open(directory.path(),schema,StoreOptions{memtable_bytes:512<<20,background_compaction:false,..StoreOptions::default()}).unwrap();
   table.bulk_ingest_snapshot((0..n).map(|id|row(id,text,false)).collect()).unwrap();
   if changed>0 {table.ingest_cdc((0..changed).map(|id|row(id,text,true)).collect()).unwrap();table.checkpoint().unwrap();}
   let snapshot=table.snapshot();
   for repeat in 0..4 {
    for width in [1,4] {
     for opt_in in [true,false] {
      let columns=if width==1 {vec![3]} else {vec![1,2,3,4]};
      ProjectedScanStream::experiment_counts(true);
      let start=Instant::now();
      let mut stream=snapshot.scan_projected_range_stream(&key(0,text),&key(n,text),&columns).unwrap().unwrap();
      if opt_in {stream.enable_memtable_overlay(&[1]);}
      let eligible=stream.experiment_eligible();
      let mut chunks=Vec::new();
      loop {let batch=stream.next_column_chunks(4,256<<20).unwrap();if batch.is_empty(){break;}chunks.extend(batch);}
      let elapsed=start.elapsed().as_secs_f64()*1000.;
      let counts=ProjectedScanStream::experiment_counts(false);
      let mut index=0;
      for chunk in chunks {for r in 0..chunk.row_count(){while index<changed && index%19==0 {index+=1;}
       let expected=row(index,text,index<changed);
       for (c,id) in columns.iter().enumerate(){assert_eq!(chunk.columns()[c].value_at(r).unwrap(),expected.values()[(*id-1)as usize]);}
       index+=1;
      }}
      while index<changed && index%19==0 {index+=1;}assert_eq!(index,n);
      if changed>0 {if text || !opt_in {assert!(!eligible);assert_eq!(counts.0,0);assert!(counts.1>0);}else{assert!(eligible);assert!(counts.0>0);assert_eq!(counts.1,0);}}
      println!("{}",serde_json::json!({"text":text,"changed":changed,"width":width,"opt_in":opt_in,"repeat":repeat,"ms":elapsed,"eligible":eligible,"overlay_slices":counts.0,"merge_parts":counts.1,"correct":true}));
     }
    }
   }
  }
 }
}
