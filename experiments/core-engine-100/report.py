import json, math
from pathlib import Path
from statistics import median
lab=Path(__import__('sys').argv[1])
screen=json.loads((lab/'evidence/summary.json').read_text())
raw=[json.loads(x) for x in (lab/'evidence/raw.jsonl').read_text().splitlines()]
keys={(r['case'],r['variant'],r['rows'],r['scenario'],r['seed']) for r in raw}
assert len(keys)==len(raw)==990
assert all(r['correct'] and r['restart_correct'] and len(r['phases'])==8 and all(p['correct'] for p in r['phases']) and r['phases'][6]['compacted_inputs']>0 for r in raw)
labels=['Scan/filter/project','Version/tombstone resolution','Low-cardinality grouping','High-cardinality grouping/top-K','Join/group aggregation','Exact grouped distinct','Ordered LIMIT','Bounded rolling windows','Nullable IN/NOT IN','Correlated aggregates']
selected=json.loads((lab/'confirmation-selection.json').read_text())['selection']
rows=[];holdout=[]
for choice in selected:
 c=choice['case'];v=choice['variant']
 s=next(r for r in screen['results'] if r['case']==c and r['variant']==v)
 h=json.loads((lab/f'confirmation-{c}/summary.json').read_text())
 assert h['processes']==6 and h['alternatives']==1
 h=h['results'][0];holdout.append(h)
 status='candidate for integration' if min(h['scenario_medians'].values())>=1.05 and h['changing_query_speedup']>=1.10 and h['changing_cycle_speedup']>=1.05 and s['changing_query_speedup']>=1.10 else 'weak or distribution-dependent'
 rows.append(f"| {c}. {labels[c-1]} | {v}: {choice['name']} | {s['changing_query_speedup']:.2f}× | {h['changing_query_speedup']:.2f}× | {h['changing_cycle_speedup']:.2f}× | {min(h['scenario_medians'].values()):.2f}× | {status} |")
ref=[r for r in raw if r['variant']==0]
active=[p for r in raw for p in r['phases'][1:7]]
lines=['# Update-aware core engine experiments','','The requested 100 alternatives have been implemented and screened against real changing','TableStore snapshots. This completes an **algorithm screen**, not 100 installed engine','optimizations. No production acceleration or breakthrough is claimed.','',
'## Results selected from changing data','','Ratios are against the lab reference, not the current Pintail SQL executor. A ratio above','1 is faster. Each query includes the projected storage scan, conversion into the lab row','format, candidate construction and result production. The cycle also waits for the','concurrent writer/flush/compaction. All maintained structures are rebuilt and charged;','there is no free persistent index or cache.','',
'| Workload | Selected approach | Screen query | Holdout query | Holdout cycle | Worst holdout distribution | Assessment |','|---|---|---:|---:|---:|---:|---|',*rows,'',
'The screen uses three seeds in each of three distributions at 100,000 invented rows.','The selected alternatives were fixed before confirmation, which uses one fresh seed','per distribution at 200,000 rows. Three holdout samples per arm are a scale/distribution','check, not a confidence interval or evidence for a production tail-latency SLO.','',
'## Coverage and evidence','','- 10 workloads × 10 alternatives, plus 10 separate controls. [Full inventory](APPROACHES.md).','- Screen: 990 sequential processes and 7,920 measured/validated snapshots.','- Confirmation: 60 processes and 480 measured/validated snapshots, selected before holdout.','- Every process additionally checks the old pinned view after a concurrent write, the','  newly committed view and restart/reopen equality. Compaction must actually merge inputs.','- Each trajectory covers inserts, NULL transitions, predicate/value and grouping/join-key','  updates, deletes, duplicate/stale versions before tombstone retirement, sparse/dense','  tails, overlapping flushed segments and compaction.','- [All 100 outcomes](evidence/RESULTS.md), [raw screen records](evidence/raw.jsonl),','  [source and binary provenance](evidence/provenance.json), [selection rule](confirmation-selection.json).','',
'## Cost of changing storage','','These are medians across the ten lab controls, three distributions and three seeds.','The scan figure includes storage decoding and the lab materialization adapter, so it','must not be presented as a measurement of storage decoding alone.','',
'| Snapshot state | Scan + adapter ms | Query ms | Concurrent writer/maintenance ms | Full cycle ms |','|---|---:|---:|---:|---:|']
for i in range(8):
 p=[r['phases'][i] for r in ref]
 lines.append('| '+p[0]['phase']+' | '+' | '.join(f'{median(x[k] for x in p):.2f}' for k in ['scan_ms','query_ms','writer_ms','cycle_ms'])+' |')
lines += ['',f"Actual writer/query overlap was nonzero in {sum(p['writer_overlap_ms']>0 for p in active):,} of {len(active):,} changing-state readings. The raw records include overlap duration; a short writer is not represented as sustained load.",'',
'## Actual engine and MySQL checks','','`engine-evidence/` contains the ten equivalent SQL queries through the real parser,','binder, optimizer and executor at all eight states and three distributions, with','settled-result memoization disabled. SQL anchors run between mutation batches, not',
'concurrently with their writer. Those timings include SQL planning and textual','result rendering; they are contextual baselines, not paired speedup comparisons with','the external kernels. `oracle-evidence/result.json` records the isolated MySQL 8.4','transactional comparison. Native binlog delivery and source-to-query lag are not covered','by direct decoded ingestion, and no integrated optimization is certified by this oracle.','',
'## Unresolved correctness boundary','','[F1](FINDINGS.md) preserves a minimal reproducer: an ancient version submitted directly','to the store after full compaction has retired a deletion marker can resurrect the key.','Native CDC reachability has not been established. The performance matrix excludes that','invalid state and never counts it as a passing replay test.','',
'## Interpretation limits','','These are finite batches with one reader and one writer, not a sustained update-rate','soak, multi-client admission test or cross-table transaction test. Join inputs are mutable','views of one table. Integer domains are bounded; dense strategies need guarded fallbacks.','String collations, DECIMAL, ENUM, schema evolution and spill budgets need integration','coverage. Peak RSS includes fixture, independent model and correctness checks; it is not','candidate-only memory. The system allocator differs from the shipped allocator.','Automatic background compaction is disabled for repeatable phase boundaries; explicit',
'compaction races the query on the writer thread. Cycle time is finite-batch service',
'cost, not sustainable update throughput or native CDC lag.',
'Reference computation precedes each timed query and can warm data/cache state.',
'Every kernel receives a full four-column scan. These measurements do not test physical',
'predicate pushdown, operator/scan fusion, a maintained persistent index or incremental',
'aggregate repair. The common adapter can hide gains that require such integration.','',
'Measurements ran as native processes in an isolated checkout on an otherwise idle build','host, with no Docker benchmark on that host: CPU model, toolchain, affinity and hashes','are in provenance. A deployment address is intentionally not part of public evidence.','']
regimes=json.loads((lab/'regime-selection.json').read_text())['selection']
regime_lines=['## Distribution-specific confirmations','','An overall median can hide an improvement limited to hot-key data. Before these','extra runs, candidates were selected independently per distribution when the screen','query ratio was at least 1.20 and the cycle ratio at least 1.05. Each selected arm','was then compared with its control at 200,000 rows using three additional fresh seeds.','These ratios still compare external prototypes with lab controls, not installed SQL.','', '| Workload | Distribution | Approach | Screen query | Fresh query | Fresh cycle | Minimum fresh process query |','|---|---|---|---:|---:|---:|---:|']
for item in regimes:
    result=json.loads((lab/f"regime-{item['case']}-{item['scenario']}/summary.json").read_text())
    assert result['processes']==6 and result['alternatives']==1
    result=result['results'][0]
    regime_lines.append(f"| {labels[item['case']-1]} | {['uniform','hot-key skew','key-clustered'][item['scenario']]} | {item['name']} | {item['screen_query_ratio']:.2f}× | {result['changing_query_speedup']:.2f}× | {result['changing_cycle_speedup']:.2f}× | {result['min_process_speedup']:.2f}× |")
regime_lines+=['',f"These add {6*len(regimes)} processes and {48*len(regimes)} checked snapshots. Selection is recorded in [regime-selection.json](regime-selection.json).",'']
position=lines.index('## Coverage and evidence')
lines[position:position]=regime_lines
(lab/'RESULTS.md').write_text('\n'.join(lines))
(lab/'confirmation-summary.json').write_text(json.dumps(holdout,indent=2)+'\n')
# Raw latency and memory distribution remains descriptive, explicitly not an SLO.
def p95(values):return sorted(values)[math.ceil(.95*len(values))-1]
lat=['# Descriptive changing-state latency and memory','','Each row pools six changing states across nine independent process fixtures. The','nearest-rank p95 mixes those different states and is descriptive only, not a steady','arrival-rate or production SLO estimate. RSS is process high-water including fixture,','reference/model, scans and validation, not a reservation charged to the candidate.','', '| Case | Variant | Query median ms | Query pooled p95 ms | Writer median ms | Cycle median ms | Median process peak KiB |','|---|---|---:|---:|---:|---:|---:|']
for c in range(1,11):
 for v in range(11):
  rr=[r for r in raw if r['case']==c and r['variant']==v];pp=[p for r in rr for p in r['phases'][1:7]]
  lat.append(f"| {c} | {v} | {median(p['query_ms'] for p in pp):.2f} | {p95([p['query_ms'] for p in pp]):.2f} | {median(p['writer_ms'] for p in pp):.2f} | {median(p['cycle_ms'] for p in pp):.2f} | {median(int(r['process_peak'].split()[1]) for r in rr):.0f} |")
(lab/'evidence/LATENCY.md').write_text('\n'.join(lat)+'\n')
print('Verified complete screen and confirmation; wrote report.')
