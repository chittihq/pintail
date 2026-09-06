import json, os, pathlib, shutil, statistics, subprocess, sys
root=pathlib.Path(__file__).resolve().parents[1]
out=pathlib.Path(os.environ["SNAPSHOT_BENCH_OUTPUT"]).resolve()
out.mkdir(parents=True, exist_ok=True)
binary=root/"target/release/examples/throughput"
cases=[("single","single",4,100000,False,False),("multi_serial","multi",4,100000,False,False),("multi_threads","multi",4,100000,True,False),("ranges_threads","ranges",4,100000,True,False),("composite_tuple","composite",1,10000,False,False),("composite_expanded","composite",1,10000,False,True)]
results=[]
for repeat in range(3):
 for label,mode,workers,chunk,threads,expand in (cases if repeat%2==0 else cases[::-1]):
  name=f"matrix-{label}-{repeat}"
  dest=out/name
  env=os.environ.copy()
  env.update(SNAPSHOT_BENCH_DSN=os.environ["SNAPSHOT_BENCH_DSN"],SNAPSHOT_BENCH_CHUNK_ROWS=str(chunk),PINTAIL_SNAPSHOT_PROFILE="1")
  for key,enabled in [("PINTAIL_SNAPSHOT_THREADS",threads),("PINTAIL_SNAPSHOT_EXPAND_KEYS",expand)]:
   env.pop(key,None)
   if enabled: env[key]="1"
  with (out/(name+".log")).open("w") as log:
   subprocess.run([str(binary),mode,str(workers),str(dest)],env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
  result=next(json.loads(line) for line in (out/(name+".log")).read_text().splitlines() if line.startswith('{'))
  assert result['verified'] and result['rows']==1000000
  result.update(case=label,repeat=repeat,chunk_rows=chunk)
  results.append(result)
  (out/"matrix-results.json").write_text(json.dumps(results,indent=2)+"\n")
  shutil.rmtree(dest)
summary={label:statistics.median(x['seconds'] for x in results if x['case']==label) for label,*_ in cases}
summary['thread_speedup']=summary['multi_serial']/summary['multi_threads']
summary['range_speedup']=summary['single']/summary['ranges_threads']
summary['predicate_speedup']=summary['composite_tuple']/summary['composite_expanded']
(out/'matrix-summary.json').write_text(json.dumps(summary,indent=2)+"\n")
assert summary['thread_speedup']>1.3, summary
assert summary['range_speedup']>1.3, summary
assert summary['predicate_speedup']>1.3, summary
print('EXPERIMENT-DONE',json.dumps(summary),flush=True)
