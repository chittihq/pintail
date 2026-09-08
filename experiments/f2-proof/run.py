#!/usr/bin/env python3
"""Sequential native SQL ablations; every query races a real writer."""
import argparse,hashlib,json,os,random,subprocess
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--out',default='evidence');p.add_argument('--pk-join',action='store_true');p.add_argument('--metadata',default='1');p.add_argument('--scenarios',default='0,1,2');p.add_argument('--seeds',default='31013,41017,51031');p.add_argument('--rows',type=int,default=100000);p.add_argument('--variants',default='0,1,2,3,4,5,6');p.add_argument('--cap',type=int,default=256<<20);a=p.parse_args()
root=Path(__file__).resolve().parents[2];out=Path(__file__).resolve().parent/a.out;out.mkdir(parents=True,exist_ok=True);raw=out/'raw.jsonl'
if raw.exists():raise SystemExit('Use a fresh output directory; existing evidence is immutable')
binary=root/'experiments/core-engine-100/target/release/proof'
files=[*sorted((root/'crates').rglob('*.rs')),*sorted((root/'experiments/core-engine-100/src').rglob('*.rs')),root/'experiments/core-engine-100/Cargo.lock',Path(__file__)]
(out/'provenance.json').write_text(json.dumps({'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'source_sha256':{str(f.relative_to(root)):hashlib.sha256(f.read_bytes()).hexdigest() for f in files},'parameters':vars(a),'cpus':'0-7','rayon_workers':4,'scan_workers':4,'settled_memo_disabled':True,'profiling':True,'source_transport':'decoded CDC, not binlog','metadata_modes':{'0':'exact count, no key','1':'snapshot-time estimated count + real PK id','2':'exact count + real PK id','3':'snapshot-time estimated count, no key'}},indent=2)+'\n')
jobs=[(v,a.rows,sc,seed,meta,a.cap) for meta in map(int,a.metadata.split(',')) for sc in map(int,a.scenarios.split(',')) for seed in map(int,a.seeds.split(',')) for v in map(int,a.variants.split(','))]
random.Random(781).shuffle(jobs)
for job in jobs:
 env=dict(os.environ,PINTAIL_DISABLE_SETTLED_MEMO='1',PINTAIL_PROOF_ESTIMATE=str(a.rows),RAYON_NUM_THREADS='4',PINTAIL_SCAN_THREADS='4')
 if a.pk_join:env['PINTAIL_PROOF_PK_JOIN']='1'
 r=subprocess.run(['taskset','-c','0-7',str(binary),*map(str,job)],env=env,text=True,capture_output=True,timeout=180)
 if r.returncode:
  (out/'failure.json').write_text(json.dumps({'job':job,'error':r.stderr.replace(str(root),'<checkout>')},indent=2));raise SystemExit('F2-PROOF-FAIL')
 with raw.open('a') as f:f.write(json.dumps(json.loads(r.stdout),separators=(',',':'))+'\n')
print(f'F2-PROOF-DONE processes={len(jobs)}',flush=True)
