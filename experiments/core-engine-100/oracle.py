#!/usr/bin/env python3
"""Own one isolated MySQL container; replay committed mutations and compare SQL."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid
p=argparse.ArgumentParser()
p.add_argument('--rows',type=int,default=1000)
p.add_argument('--cases',default='1,2,3,4,5,6,7,8,9,10')
p.add_argument('--scenarios',default='0,1,2')
p.add_argument('--out',default='oracle-evidence')
a=p.parse_args()
cases=list(map(int,a.cases.split(',')))
assert cases and all(1 <= c <= 10 for c in cases)
root=Path(__file__).resolve().parent
out=root/a.out
out.mkdir(parents=True,exist_ok=True)
name='pintail-core100-oracle-'+uuid.uuid4().hex[:10]
owned=False
results=[]
def docker(args,**kw):
    result=subprocess.run(['docker',*args],text=True,capture_output=True,**kw)
    if result.returncode:
        (out/'docker-error.json').write_text(json.dumps({'command':args,'exit':result.returncode,'stderr':result.stderr,'stdout':result.stdout},indent=2)+'\n')
        raise RuntimeError(f'docker command failed: {result.stderr.strip()}')
    return result
try:
    docker(['run','-d','--name',name,'--label','pintail.harness=core100','-e','MYSQL_ALLOW_EMPTY_PASSWORD=yes','mysql:8.4','--binlog-format=ROW','--binlog-row-image=FULL','--server-id=812'])
    owned=True
    deadline=time.monotonic()+120
    while True:
        r=subprocess.run(['docker','exec',name,'mysql','-h127.0.0.1','-N','-uroot','-e','SELECT 1'],capture_output=True)
        if r.returncode==0 and r.stdout.strip()==b'1':break
        if time.monotonic()>deadline:raise RuntimeError('MySQL readiness timeout')
        time.sleep(1)
    version=docker(['exec',name,'mysql','-N','-uroot','-e','SELECT VERSION()']).stdout.strip()
    image=docker(['inspect','--format','{{.Image}}',name]).stdout.strip()
    for scenario in map(int,a.scenarios.split(',')):
        export=out/f'scenario-{scenario}'
        env=dict(os.environ,RAYON_NUM_THREADS='4',PINTAIL_SCAN_THREADS='4')
        if len(cases)==1:env['PINTAIL_LAB_CASE']=str(cases[0])
        r=subprocess.run([str(root/'target/release/anchors'),str(a.rows),str(scenario),'41',str(export)],check=True,capture_output=True,text=True,env=env)
        anchors=json.loads(r.stdout)
        for phase in range(8):
            docker(['exec','-i',name,'mysql','-uroot'],input=(export/f'phase-{phase}-load.sql').read_text())
            for case in cases:
                sql=(export/f'phase-{phase}-case-{case}.sql').read_text()
                actual=docker(['exec','-i',name,'mysql','-N','-B','--raw','-uroot','lab'],input=sql).stdout
                expected=(export/f'phase-{phase}-case-{case}.tsv').read_text()
                # TSV cannot distinguish zero rows from a blank line in the exporter.
                if expected=='\n':expected=''
                if actual!=expected:
                    (export/f'phase-{phase}-case-{case}-mysql.tsv').write_text(actual)
                    raise AssertionError(f'MySQL mismatch scenario={scenario} phase={phase} case={case}')
                results.append({'scenario':scenario,'phase':phase,'case':case,'exact_tsv':True})
        (out/f'anchors-{scenario}.json').write_text(json.dumps(anchors,indent=2)+'\n')
    (out/'result.json').write_text(json.dumps({'mysql_version':version,'image_id':image,'checks':results,'count':len(results),'scope':'transactional MySQL oracle vs engine anchors; direct decoded CDC ingestion, not native binlog transport'},indent=2)+'\n')
    hashes={str(f.relative_to(out)):hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(out.rglob('phase-*')) if f.is_file()}
    (out/'artifact-sha256.json').write_text(json.dumps(hashes,indent=2)+'\n')
    print('CORE-100-ORACLE-DONE',flush=True)
finally:
    if owned:docker(['rm','-f',name])
