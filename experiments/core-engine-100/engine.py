#!/usr/bin/env python3
"""Record actual SQL answers and resource refusals without disguising them as passes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
p.add_argument('--rows',type=int,default=100000)
p.add_argument('--out',default='engine-evidence')
p.add_argument('--prefilter-join',action='store_true')
p.add_argument('--factorized-join',action='store_true')
a=p.parse_args()
root=Path(__file__).resolve().parent
out=root/a.out
out.mkdir(parents=True,exist_ok=True)
binary=root/'target/release/anchors'
provenance={'source_head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'runner_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),'parameters':vars(a),'query_memory_limit_bytes':256<<20,'settled_memo_disabled':True,'affinity':'0-7','rayon_workers':4,'scan_workers':4,'scope':'SQL between mutation batches; source model and textual result rendering included; resource refusals are failures, not successful timings'}
(out/'provenance.json').write_text(json.dumps(provenance,indent=2)+'\n')
passed=refused=0
for scenario in range(3):
    env=dict(os.environ,PINTAIL_DISABLE_SETTLED_MEMO='1',PINTAIL_LAB_RECORD_RESOURCE_ERRORS='1',RAYON_NUM_THREADS='4',PINTAIL_SCAN_THREADS='4')
    if a.prefilter_join:
        env.update(PINTAIL_LAB_JOIN_PREFILTER='1',PINTAIL_LAB_CASE='5')
    if a.factorized_join:
        env.update(PINTAIL_LAB_JOIN_FACTORIZED='1',PINTAIL_LAB_CASE='5')
    r=subprocess.run(['taskset','-c','0-7',str(binary),str(a.rows),str(scenario),'9901'],env=env,capture_output=True,text=True,timeout=300)
    if r.returncode:
        (out/f'scenario-{scenario}-error.txt').write_text(r.stderr.replace(str(root),'<experiment>'))
        raise SystemExit(f'CORE-100-ENGINE-FAILED scenario={scenario}')
    record=json.loads(r.stdout)
    (out/f'scenario-{scenario}.json').write_text(json.dumps(record,indent=2)+'\n')
    readings=[c for phase in record['phases'] for c in phase['cases']]
    passed+=sum(c['correct'] for c in readings)
    refused+=sum('resource_error' in c for c in readings)
summary={'exact_answers':passed,'resource_refusals':refused,'complete_success':refused==0,'scope':provenance['scope']}
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(f'CORE-100-ENGINE-RECORDED exact={passed} resource_refusals={refused}',flush=True)
