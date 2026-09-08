#!/usr/bin/env python3
"""Summarize immutable measurements; ratios use paired dirty-state totals."""
import json, statistics
from pathlib import Path
root=Path(__file__).resolve().parent
result={}
for folder in ['evidence','pk-evidence','metadata-evidence']:
 records=[json.loads(line) for line in (root/folder/'raw.jsonl').read_text().splitlines()]
 summary={'processes':len(records),'states':sum(len(r['phases']) for r in records),'arms':[]}
 for metadata,scenario,variant in sorted({(r['metadata'],r['scenario'],r['variant']) for r in records}):
  arm=[r for r in records if (r['metadata'],r['scenario'],r['variant'])==(metadata,scenario,variant)]
  phases=[p for r in arm for p in r['phases']]
  item={'metadata':metadata,'scenario':scenario,'variant':variant,'name':arm[0]['name'],'successful':sum(p['correct'] for p in phases),'total':len(phases)}
  ratios={}
  for metric in ['query_ms','cycle_ms']:
   pairs=[]
   for r in arm:
    base=next((b for b in records if (b['metadata'],b['scenario'],b['seed'],b['variant'])==(metadata,scenario,r['seed'],3)),None)
    if base and all(p['correct'] for p in r['phases'][1:]+base['phases'][1:]):
     pairs.append(sum(p[metric] for p in base['phases'][1:])/sum(p[metric] for p in r['phases'][1:]))
   if pairs:ratios[metric]={'median':statistics.median(pairs),'min':min(pairs),'max':max(pairs),'pairs':pairs}
  item['filter_project_over_arm']=ratios;summary['arms'].append(item)
 result[folder]=summary
(root/'summary.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({k:{x:v[x] for x in ['processes','states']} for k,v in result.items()}))
