#!/usr/bin/env python3
"""Keep all arms; rank only changing-state measurements against paired controls."""
import argparse
import json
from pathlib import Path
from statistics import median
p=argparse.ArgumentParser()
p.add_argument('directory',nargs='?',default='evidence')
a=p.parse_args()
root=Path(__file__).resolve().parent
out=root/a.directory
records=[json.loads(s) for s in (out/'raw.jsonl').read_text().splitlines()]
reference={(r['case'],r['rows'],r['scenario'],r['seed']):r for r in records if r['variant']==0}
active={'sparse-memtable','sparse-flushed-overlap','dense-hot-memtable','dense-flushed-overlap','mixed-overlap','stale-replay'}
results=[]
for case in sorted({r['case'] for r in records}):
    for variant in range(1,11):
        arms=[r for r in records if r['case']==case and r['variant']==variant]
        if not arms: continue
        ratios=[]; cycles=[]; operators=[]; phases={}; by_scenario={}
        for r in arms:
            control=reference[(case,r['rows'],r['scenario'],r['seed'])]
            controls={x['phase']:x for x in control['phases']}
            q0=q1=c0=c1=o0=o1=0.
            for reading in r['phases']:
                name=reading['phase']; base=controls[name]
                phases.setdefault(name,[]).append(base['query_ms']/max(reading['query_ms'],1e-9))
                if name not in active:continue
                q0+=base['query_ms'];q1+=reading['query_ms'];c0+=base['cycle_ms'];c1+=reading['cycle_ms'];o0+=base['operator_ms'];o1+=reading['operator_ms']
            ratio=q0/q1;ratios.append(ratio);cycles.append(c0/c1);operators.append(o0/o1)
            by_scenario.setdefault(r['scenario'],[]).append(ratio)
        results.append({'case':case,'variant':variant,'name':arms[0]['name'],'processes':len(arms),'changing_query_speedup':median(ratios),'changing_cycle_speedup':median(cycles),'operator_only_speedup':median(operators),'min_process_speedup':min(ratios),'max_process_speedup':max(ratios),'scenario_medians':{k:median(v) for k,v in by_scenario.items()},'phase_medians':{k:median(v) for k,v in phases.items()}})
summary={'processes':len(records),'validated_snapshots':sum(len(r['phases']) for r in records),'alternatives':len(results),'results':results}
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
lines=['# Changing-data experiment results','','Ratios compare each algorithm with its explicit reference over the same changing','fixture. They are **not speedups over the current Pintail SQL executor**. The','query figure includes a real storage scan and materialization followed by the','experimental operator. Cycle time also waits for concurrent mutation/maintenance.','Setup for candidate indexes and intermediate state is included; initial fixture','creation and independent correctness checks are excluded.','',f"{len(records)} processes; {summary['validated_snapshots']} checked snapshots; {len(results)} alternatives.",'','| Case | Approach | Query ratio | Cycle ratio | Operator ratio | Worst scenario median |','|---|---|---:|---:|---:|---:|']
for r in results:
    lines.append(f"| {r['case']} | {r['variant']}: {r['name']} | {r['changing_query_speedup']:.2f}× | {r['changing_cycle_speedup']:.2f}× | {r['operator_only_speedup']:.2f}× | {min(r['scenario_medians'].values()):.2f}× |")
lines+=['','## Strongest observed alternative per workload','','Selection is exploratory; these same samples selected the winners. Confirm on','new seeds, larger tables and the real SQL path before adopting anything.','']
for case in sorted({r['case'] for r in results}):
    best=max((r for r in results if r['case']==case),key=lambda r:r['changing_query_speedup'])
    lines.append(f"- Case {case}: {best['name']}, {best['changing_query_speedup']:.2f}× query, {best['changing_cycle_speedup']:.2f}× cycle.")
(out/'RESULTS.md').write_text('\n'.join(lines)+'\n')
print(json.dumps({k:v for k,v in summary.items() if k!='results'}))
