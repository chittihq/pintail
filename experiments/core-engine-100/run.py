#!/usr/bin/env python3
"""Sequential changing-data experiment matrix; run from the build checkout."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[1]
p = argparse.ArgumentParser()
p.add_argument('--rows', type=int, default=100000)
p.add_argument('--repeats', type=int, default=3)
p.add_argument('--seed-base', type=int, default=41)
p.add_argument('--cases', default='1,2,3,4,5,6,7,8,9,10')
p.add_argument('--scenarios', default='0,1,2')
p.add_argument('--variants', default=','.join(map(str, range(11))))
p.add_argument('--out', default='evidence')
p.add_argument('--cpus', default='0-7')
p.add_argument('--resume', action='store_true')
a = p.parse_args()
out = ROOT / a.out
out.mkdir(parents=True, exist_ok=True)
raw = out / 'raw.jsonl'
if raw.exists() and not a.resume:
    raise SystemExit('Evidence already exists; use a new --out or explicit --resume')
binary = ROOT / 'target/release/live'
if not binary.exists():
    raise SystemExit('Build the release binaries first with CARGO_TARGET_DIR=target inside the lab')
files = [ROOT/'Cargo.toml', ROOT/'Cargo.lock', Path(__file__)]
files += sorted((ROOT/'src').rglob('*.rs'))
files += sorted((REPO/'crates').rglob('*.rs'))
hashes = {str(f.relative_to(REPO)): hashlib.sha256(f.read_bytes()).hexdigest() for f in files}
provenance = {
    'started_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'source_head': subprocess.check_output(['git','rev-parse','HEAD'], cwd=REPO, text=True).strip(),
    'machine': platform.machine(), 'os': platform.system(), 'kernel': platform.release(),
    'cpu_model': next((s.split(':',1)[1].strip() for s in Path('/proc/cpuinfo').read_text().splitlines() if s.startswith('model name')), 'unknown'),
    'affinity': a.cpus, 'rayon_workers': 4, 'scan_workers': 4,
    'rustc': subprocess.check_output([str(Path.home()/'.cargo/bin/rustc'),'--version'], text=True).strip(),
    'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
    'source_sha256': hashes, 'parameters': vars(a),
    'allocator': 'system', 'network_measurement': False,
    'scope': 'real storage scan plus external algorithm; not an installed SQL operator',
}
provenance_path = out/'provenance.json'
if a.resume:
    previous = json.loads(provenance_path.read_text())
    if previous['binary_sha256'] != provenance['binary_sha256'] or previous['source_sha256'] != hashes:
        raise SystemExit('Cannot resume evidence with a changed binary or source')
else:
    provenance_path.write_text(json.dumps(provenance, indent=2)+'\n')
completed = set()
if raw.exists():
    for line in raw.read_text().splitlines():
        r=json.loads(line)
        if r.get('correct'):
            completed.add((r['case'],r['variant'],r['rows'],r['scenario'],r['seed']))
env = dict(os.environ, RAYON_NUM_THREADS='4', PINTAIL_SCAN_THREADS='4')
failures = []
for case in map(int,a.cases.split(',')):
    jobs=[]
    for scenario in map(int,a.scenarios.split(',')):
        for repeat in range(a.repeats):
            seed=a.seed_base+repeat*83
            arms=list(map(int,a.variants.split(',')))
            random.Random(9100+case*100+scenario*10+repeat).shuffle(arms)
            jobs += [(case,v,a.rows,scenario,seed) for v in arms]
    for job in jobs:
        if job in completed:
            continue
        cmd=['taskset','-c',a.cpus,str(binary),*map(str,job)]
        try:
            r=subprocess.run(cmd,env=env,text=True,capture_output=True,timeout=180)
            if r.returncode:
                error=r.stderr.replace(str(REPO),'<checkout>')
                failures.append({'job':job,'exit':r.returncode,'error':error[:12000]})
                (out/'failures.json').write_text(json.dumps(failures,indent=2)+'\n')
                raise SystemExit(f'CORE-100-FAIL case={case} variant={job[1]} (see failures.json)')
            record=json.loads(r.stdout.strip().splitlines()[-1])
        except subprocess.TimeoutExpired:
            failures.append({'job':job,'error':'180 second timeout'})
            (out/'failures.json').write_text(json.dumps(failures,indent=2)+'\n')
            raise SystemExit(f'CORE-100-FAIL timeout case={case} variant={job[1]}')
        with raw.open('a') as f:
            f.write(json.dumps(record,separators=(',',':'))+'\n')
            f.flush()
    print(f'case {case}: complete',flush=True)
print('CORE-100-DONE',flush=True)
