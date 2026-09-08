#!/usr/bin/env python3
"""Confirm preselected screen winners on fresh, larger changing fixtures."""
import json
from pathlib import Path
import subprocess
root=Path(__file__).resolve().parent
subprocess.run(['python3',str(root/'summarize.py')],check=True)
screen=json.loads((root/'evidence/summary.json').read_text())
assert screen['processes']==990 and screen['alternatives']==100
selection=[]
for case in range(1,11):
    best=max((r for r in screen['results'] if r['case']==case),key=lambda r:r['changing_query_speedup'])
    selection.append({'case':case,'variant':best['variant'],'name':best['name']})
path=root/'confirmation-selection.json'
record={'rule':'highest screen median changing-query ratio, selected before holdout; one fresh seed per distribution at twice the rows','selection':selection}
if path.exists():
    assert json.loads(path.read_text())==record, 'Selection changed after confirmation'
else:
    path.write_text(json.dumps(record,indent=2)+'\n')
for item in selection:
    out=f"confirmation-{item['case']}"
    subprocess.run(['python3',str(root/'run.py'),'--rows','200000','--repeats','1','--seed-base','9901','--cases',str(item['case']),'--variants',f"0,{item['variant']}",'--out',out],check=True)
    subprocess.run(['python3',str(root/'summarize.py'),out],check=True)
print('CORE-100-CONFIRMATION-DONE')
