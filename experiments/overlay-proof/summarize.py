#!/usr/bin/env python3
import json,statistics
from pathlib import Path
here=Path(__file__).resolve().parent
records=[json.loads(line) for line in (here/'raw.jsonl').read_text().splitlines()]
assert len(records)==128
result=[]
for text in [False,True]:
 for width in [1,4]:
  for changed in [0,1,1000,20000]:
   for opt in [True,False]:
    rows=[r for r in records if (r['text'],r['width'],r['changed'],r['opt_in'])==(text,width,changed,opt)]
    assert len(rows)==4 and all(r['correct'] for r in rows)
    result.append({'text':text,'width':width,'changed':changed,'opt_in':opt,'median_ms':statistics.median(r['ms'] for r in rows if r['repeat']>0),'overlay_slices':sorted({r['overlay_slices'] for r in rows}),'merge_parts':sorted({r['merge_parts'] for r in rows})})
(here/'summary.json').write_text(json.dumps(result,indent=2)+'\n')
for r in result:
 if r['opt_in']:
  base=next(b['median_ms'] for b in result if (b['text'],b['width'],b['changed'],b['opt_in'])==(r['text'],r['width'],0,True))
  print(r['text'],r['width'],r['changed'],round(r['median_ms'],3),round(r['median_ms']/base,2),r['overlay_slices'],r['merge_parts'])
