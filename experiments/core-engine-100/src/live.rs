//! Actual WAL/segment reads with a concurrent writer and an independent model.
use crate::{Data,Row,merge::Version};
use pintail_store::{StoreOptions,TableSnapshot,TableStore};
use pintail_types::{Column,DataType,KeyPart,PrimaryKey,StoredRow,TableSchema,Value};
use std::collections::BTreeMap;

pub fn schema()->TableSchema{TableSchema::new(1,vec![Column::new(1,"id",DataType::UInt64,false),Column::new(2,"k",DataType::UInt64,false),Column::new(3,"g",DataType::UInt64,false),Column::new(4,"v",DataType::Int64,true),Column::new(5,"padding",DataType::Utf8,false)]).unwrap()}
pub fn stored(v:Version)->StoredRow{let r=v.row;StoredRow::new(PrimaryKey::new(vec![KeyPart::UInt64(r.id as u64)]).unwrap(),vec![Value::UInt64(r.id as u64),Value::UInt64(r.key as u64),Value::UInt64(r.low as u64),if r.valid{Value::Int64(r.value)}else{Value::Null},Value::Utf8(format!("invented payload {:0128}",r.id))],v.version,v.dead)}
pub fn read(snapshot:&TableSnapshot,template:&Data)->Data{
 let low=PrimaryKey::new(vec![KeyPart::UInt64(0)]).unwrap();let high=PrimaryKey::new(vec![KeyPart::UInt64(u64::MAX)]).unwrap();
 let projected=snapshot.scan_projected_range_stream(&low,&high,&[1,2,3,4]).unwrap();let fallback=projected.is_none();
 let mut rows=Vec::new();if let Some(mut stream)=projected{loop{let chunks=stream.next_column_chunks(4,256<<20).unwrap();if chunks.is_empty(){break;}for chunk in chunks{for i in 0..chunk.row_count(){let c=chunk.columns();let integer=|j:usize|match c[j].value_at(i).unwrap(){Value::UInt64(x)=>x as usize,other=>panic!("integer {other:?}")};let(value,valid)=match c[3].value_at(i).unwrap(){Value::Int64(x)=>(x,true),Value::Null=>(0,false),other=>panic!("value {other:?}")};rows.push(Row{id:integer(0),key:integer(1),low:integer(2),value,valid});}}}}
 if fallback { for stored in snapshot.scan().unwrap() { let v=stored.values(); let integer=|i:usize|match &v[i]{Value::UInt64(x)=>*x as usize,_=>panic!("integer")};let(value,valid)=match &v[3]{Value::Int64(x)=>(*x,true),Value::Null=>(0,false),_=>panic!("value")};rows.push(Row{id:integer(0),key:integer(1),low:integer(2),value,valid});} }
 rows.sort_unstable_by_key(|r|r.id);Data{rows,domain:template.domain,low_domain:template.low_domain,scenario:template.scenario,history:template.history.clone()}
}
pub struct Fixture{pub directory:tempfile::TempDir,pub table:TableStore,pub model:BTreeMap<usize,Version>,pub template:Data,pub history:Vec<Version>,pub initial_rows:usize}
impl Fixture{
 pub fn new(n:usize,seed:u64,scenario:usize)->Self{
  let mut template=Data::new(n,seed,scenario);for r in &mut template.rows{if !r.valid{r.value=0;}}
  let history:Vec<_>=template.rows.iter().map(|&row|Version{row,version:1,dead:false}).collect();let model=history.iter().map(|v|(v.row.id,*v)).collect();let directory=tempfile::tempdir().unwrap();let mut table=TableStore::open(directory.path(),schema(),StoreOptions{memtable_bytes:512<<20,background_compaction:false,..StoreOptions::default()}).unwrap();table.bulk_ingest_snapshot(history.iter().copied().map(stored).collect()).unwrap();Self{directory,table,model,template,history,initial_rows:n}
 }
 pub fn expected(&self)->Data{Data{rows:self.model.values().filter(|v|!v.dead).map(|v|v.row).collect(),domain:self.template.domain,low_domain:self.template.low_domain,scenario:self.template.scenario,history:Some(self.history.clone())}}
 pub fn events(&self,phase:usize)->Vec<Version>{
  if phase==2||phase==4||phase==6||phase==8{return Vec::new();}
  if phase==7{return self.history.iter().take(32).map(|v|Version{version:0,..*v}).collect();}
  let dense=phase==3;let count=if dense{self.initial_rows/5}else{(self.initial_rows/100).max(1)};let mut events=Vec::new();
  for j in 0..count{let id=if dense{j}else{(j*97+phase*13)%self.initial_rows.max(1)};if let Some(old)=self.model.get(&id){let mut row=old.row;row.key=(row.key+phase*7)%self.template.domain;row.low=row.key%self.template.low_domain;row.valid=!j.is_multiple_of(9);row.value=if row.valid{((row.value+10000+phase as i64*127)%20001)-10000}else{0};events.push(Version{row,version:phase as u64+2,dead:j.is_multiple_of(19)});}}
  for j in 0..(self.initial_rows/1000).max(1){let id=self.initial_rows+phase*(self.initial_rows/1000).max(1)+j;let row=Row{id,key:j%self.template.domain,low:j%self.template.low_domain,value:j as i64%10000,valid:true};events.push(Version{row,version:phase as u64+2,dead:false});}
  if let Some(v)=events.first().copied(){events.push(v);}events
 }
 pub fn update_model(&mut self,events:&[Version]){for &v in events{self.history.push(v);self.model.entry(v.row.id).and_modify(|old|{if v.version>old.version{*old=v;}}).or_insert(v);}}
}
pub fn advance(table:&mut TableStore,events:&[Version],phase:usize)->(usize,usize){if !events.is_empty(){table.ingest_cdc(events.iter().copied().map(stored).collect()).unwrap();table.checkpoint().unwrap();}let mut compacted=0;if [2,4,5].contains(&phase){table.flush().unwrap();}if phase==6{compacted=table.compact().unwrap().input_segments();}let segments=table.compaction_status().unwrap().segment_count();(segments,compacted)}
