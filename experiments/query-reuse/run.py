#!/usr/bin/env python3
"""Sequential, shuffled fresh-process experiments with all readings retained."""
import hashlib
import json
import os
from pathlib import Path
import random
import statistics
import sys
import subprocess

root = Path(__file__).resolve().parent
out = root / "evidence"
out.mkdir(exist_ok=True)
cpus = sorted(os.sched_getaffinity(0))[:4]
env = dict(os.environ, RAYON_NUM_THREADS="2")
jobs = []
for repeat in range(5):
    for clients in [1, 4, 16]:
        for duplicate in sorted(set([0, clients // 4, clients])):
            for mode in ["independent", "shared"]:
                jobs.append((repeat, ["flights", "50000", str(clients), str(duplicate), mode]))
    for relevant in ([] if "--flights-only" in sys.argv else [0, 25, 100]):
        for mode in ["uncached", "cached"]:
            jobs.append((repeat, ["epochs", "10000", str(relevant), mode]))
random.Random(9217).shuffle(jobs)
metadata = dict(base_commit=(root / "BASE_COMMIT").read_text().strip(),
                rustc=subprocess.check_output([str(Path.home()/".cargo/bin/rustc"), "--version"], text=True).strip(),
                allocator="tikv-jemallocator 0.6", logical_cpus=os.cpu_count(), affinity_cpus=len(cpus),
                rayon_threads=2, independent_repetitions=5, seed=9217,
                cpu_ticks_per_second=os.sysconf("SC_CLK_TCK"),
                overlapping_memtable=True, run_arguments=sys.argv[1:], source_sha256={str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest()
                               for p in [root/"src/main.rs", root/"src/lib.rs", root/"Cargo.lock",root/"run.py"]})
(out/"environment.json").write_text(json.dumps(metadata, indent=2)+"\n")
readings=[]
with (out/"raw.jsonl").open("w") as output:
    for repeat, args in jobs:
        command=["taskset","-c",",".join(map(str,cpus)),str(root/"target/release/query-reuse-lab"),*args]
        result=json.loads(subprocess.check_output(command,env=env,text=True,timeout=120))
        result["repeat"]=repeat
        readings.append(result)
        output.write(json.dumps(result)+"\n")
        output.flush()
summary=[]
groups={}
for row in readings:
    key=(row["experiment"],row.get("clients"),row.get("duplicates"),row.get("shared"),row.get("relevant_percent"),row.get("cached"))
    groups.setdefault(key,[]).append(row)
for key, samples in sorted(groups.items()):
    sample=samples[0]
    result={k:sample[k] for k in ["experiment","clients","duplicates","shared","relevant_percent","cached"] if k in sample}
    for name in ["elapsed_ns","cpu_ticks","sampled_query_bytes","process_peak_rss_kib","executions","followers","hits","query_ns","tracking_ns","ingest_ns","actual_relevant_updates"]:
        if name in sample:
            values=[r[name] for r in samples]
            result[name]=dict(median=statistics.median(values), min=min(values), max=max(values), samples=values)
    latencies=sorted(t for r in samples for t in r["latency_ns"])
    result["pooled_latency_ns"]={"p50":statistics.median(latencies),"p95":latencies[int(.95*(len(latencies)-1))],"p99":latencies[int(.99*(len(latencies)-1))]}
    summary.append(result)
(out/"summary.json").write_text(json.dumps(summary,indent=2)+"\n")
print(f"QUERY-REUSE-DONE {len(readings)} processes",flush=True)
