#!/usr/bin/env python3
"""Own one isolated MySQL container; replay committed mutations and compare SQL."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import uuid
p=argparse.ArgumentParser()
p.add_argument('--rows',type=int,default=1000)
p.add_argument('--scenarios',default='0,1,2')
p.add_argument('--out',default='oracle-evidence')
a=p.parse_args()
root=Path(__file__).resolve().parent
out=root/a.out
out.mkdir(parents=True,exist_ok=True)
name='pintail-core100-oracle-'+uuid.uuid4().hex[:10]
owned=False
results=[]
def docker(args,**kw):
    return subprocess.run(['docker',*args],check=True,text=True,capture_output=True,**kw)
try:
    docker(['run','-d','--name',name,'--label','pintail.harness=core100','-e','MYSQL_ALLOW_EMPTY_PASSWORD=yes','mysql:8.4','--binlog-format=ROW','--binlog-row-image=FULL','--server-id=812'])
    owned=True
    deadline=time.monotonic()+120
    while True:
        r=subprocess.run(['docker','exec',name,'mysqladmin','ping','-uroot'],capture_output=True)
        if r.returncode==0:break
        if time.monotonic()>deadline:raise RuntimeError('MySQL readiness timeout')
        time.sleep(1)
    version=docker(['exec',name,'mysql','-N','-uroot','-e','SELECT VERSION()']).stdout.strip()
    image=docker(['inspect','--format','{{.Image}}',name]).stdout.strip()
    for scenario in map(int,a.scenarios.split(',')):
        export=out/f'scenario-{scenario}'
        r=subprocess.run([str(root/'target/release/anchors'),str(a.rows),str(scenario),'41',str(export)],check=True,capture_output=True,text=True,env=dict(os.environ,RAYON_NUM_THREADS='4',PINTAIL_SCAN_THREADS='4'))
        anchors=json.loads(r.stdout)
        for phase in range(8):
            docker(['exec','-i',name,'mysql','-uroot'],input=(export/f'phase-{phase}-load.sql').read_text())
            for case in range(1,11):
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
    print('CORE-100-ORACLE-DONE',flush=True)
finally:
    if owned:docker(['rm','-f',name])
