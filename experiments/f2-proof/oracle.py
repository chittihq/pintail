#!/usr/bin/env python3
"""MySQL comparisons for every ablation plus small counterexamples to unsafe rules."""
import hashlib,json,os,subprocess,time,uuid
from pathlib import Path
root=Path(__file__).resolve().parents[2];out=Path(__file__).resolve().parent/'oracle-evidence';out.mkdir(parents=True,exist_ok=True)
name='pintail-f2-proof-'+uuid.uuid4().hex[:10];owned=False
checks=[];witnesses=[]
def docker(args,sql=None):
 r=subprocess.run(['docker',*args],input=sql,text=True,capture_output=True)
 if r.returncode:raise RuntimeError(r.stderr)
 return r.stdout
def query(sql,db='guards'):
 return docker(['exec','-i',name,'mysql','-N','-B','--raw','-uroot',db],sql)
def witness(label,sql,expected=None,other=None,equal=None):
 result=query(sql);r={'label':label,'sql':sql,'result':result}
 if expected is not None:assert result==expected,(label,result,expected)
 if other is not None:
  alt=query(other);r.update(alternative_sql=other,alternative=alt)
  assert (result==alt)==equal,(label,result,alt)
 witnesses.append(r)
try:
 docker(['run','-d','--name',name,'--label','pintail.harness=f2-proof','-e','MYSQL_ALLOW_EMPTY_PASSWORD=yes','mysql:8.4']);owned=True
 deadline=time.monotonic()+120
 while True:
  r=subprocess.run(['docker','exec',name,'mysql','-h127.0.0.1','-N','-uroot','-e','SELECT 1'],capture_output=True)
  if r.returncode==0 and r.stdout.strip()==b'1':break
  if time.monotonic()>deadline:raise RuntimeError('MySQL TCP readiness timeout')
  time.sleep(1)
 version=docker(['exec',name,'mysql','-N','-uroot','-e','SELECT VERSION()']).strip()
 image=docker(['inspect','--format','{{.Image}}',name]).strip()
 for scenario in range(3):
  export=out/f'fixture-{scenario}'
  env=dict(os.environ,PINTAIL_DISABLE_SETTLED_MEMO='1',RAYON_NUM_THREADS='4',PINTAIL_SCAN_THREADS='4',PINTAIL_PROOF_ESTIMATE='1000')
  subprocess.run([str(root/'experiments/core-engine-100/target/release/anchors'),'1000',str(scenario),'61043',str(export)],env=env,check=True,capture_output=True)
  records=[]
  for pk in [False,True]:
   e=dict(env)
   if pk:e['PINTAIL_PROOF_PK_JOIN']='1'
   for variant in range(7):
    r=subprocess.run([str(root/'experiments/core-engine-100/target/release/proof'),str(variant),'1000',str(scenario),'61043','1',str(2<<30)],env=e,check=True,capture_output=True,text=True)
    record=json.loads(r.stdout);assert all(p['correct'] for p in record['phases']), (scenario,pk,variant,[(p['phase'],p['error']) for p in record['phases'] if not p['correct']]);records.append(record)
  for phase in range(8):
   docker(['exec','-i',name,'mysql','-uroot'],(export/f'phase-{phase}-load.sql').read_text())
   for record in records:
    p=record['phases'][phase];expected=''.join('\t'.join(row)+'\n' for row in p['expected']);actual=query(p['sql'],'lab')
    assert actual==expected,(scenario,phase,record['variant'],record['pk_join'],actual,expected)
    checks.append({'scenario':scenario,'phase':phase,'variant':record['variant'],'pk_join':record['pk_join'],'exact':True})
 docker(['exec','-i',name,'mysql','-uroot'],'''CREATE DATABASE guards; USE guards;
 CREATE TABLE f(id BIGINT PRIMARY KEY,k BIGINT,v BIGINT NULL);
 INSERT INTO f VALUES(1,1,10),(2,1,NULL),(3,2,30);
 CREATE TABLE d(id BIGINT PRIMARY KEY,k BIGINT,g BIGINT);
 INSERT INTO d VALUES(1,1,7),(2,1,7),(3,2,7);
 CREATE TABLE pk_text(id VARCHAR(8) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin PRIMARY KEY);
 INSERT INTO pk_text VALUES('1'),('01');
 CREATE TABLE pk_case(id VARCHAR(8) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin PRIMARY KEY);
 INSERT INTO pk_case VALUES('a'),('A');
 CREATE TABLE uq(id BIGINT PRIMARY KEY,k BIGINT NULL UNIQUE); INSERT INTO uq VALUES(1,NULL),(2,NULL);
 CREATE TABLE cp(tenant BIGINT,k BIGINT,PRIMARY KEY(tenant,k)); INSERT INTO cp VALUES(1,1),(2,1);
 CREATE TABLE av(id BIGINT PRIMARY KEY,k BIGINT,v BIGINT); INSERT INTO av VALUES(1,1,0),(2,1,100),(3,2,100);
 CREATE TABLE one_d(id BIGINT PRIMARY KEY,g BIGINT,t BIGINT); INSERT INTO one_d VALUES(1,7,50),(2,7,50);
 ''')
 witness('multiplicity-preserving factorization needs no dimension uniqueness',
 'SELECT COUNT(*),COUNT(f.v),SUM(f.v) FROM f JOIN d ON f.k=d.k','5\t3\t50\n',
 'SELECT SUM(a.n),SUM(a.c),SUM(a.s) FROM (SELECT k,COUNT(*) n,COUNT(v) c,SUM(v) s FROM f GROUP BY k) a JOIN d ON a.k=d.k',True)
 witness('compatible complete primary-key join is at most one match',
 'SELECT MAX(n) FROM (SELECT f.id,COUNT(one_d.id) n FROM f JOIN one_d ON f.k=one_d.id GROUP BY f.id) q','1\n')
 witness('numeric coercion breaks at-most-one despite full primary-key coverage',
 'SELECT COUNT(*) FROM (SELECT 1 k) f JOIN pk_text d ON f.k=d.id','2\n')
 witness('collation override breaks at-most-one despite full primary-key coverage',
 "SELECT COUNT(*) FROM pk_case WHERE id COLLATE utf8mb4_general_ci = _utf8mb4'a' COLLATE utf8mb4_general_ci",'2\n')
 witness('partial composite-key coverage is not unique','SELECT COUNT(*) FROM cp WHERE k=1','2\n')
 witness('nullable UNIQUE plus NULL-safe equality is not unique','SELECT COUNT(*) FROM uq WHERE k <=> NULL','2\n')
 witness('average of averages changes weights','SELECT AVG(v) FROM av',other='SELECT AVG(a) FROM (SELECT k,AVG(v) a FROM av GROUP BY k) q',equal=False)
 witness('counting partial groups loses fact multiplicity','SELECT COUNT(*) FROM f','3\n','SELECT COUNT(*) FROM (SELECT k,COUNT(*) n FROM f GROUP BY k) q',False)
 witness('primary key does not license discarding a fact-dependent residual',
 'SELECT COUNT(*),SUM(av.v) FROM av JOIN one_d d ON av.k=d.id AND av.v>d.t',other='SELECT SUM(a.n),SUM(a.s) FROM (SELECT k,COUNT(*) n,SUM(v) s FROM av GROUP BY k) a JOIN one_d d ON a.k=d.id AND a.s>d.t',equal=False)
 witness('NULL partial sums cannot be replaced by zero',
 'SELECT SUM(v) FROM f WHERE id=2','NULL\n','SELECT SUM(COALESCE(s,0)) FROM (SELECT SUM(v) s FROM f WHERE id=2 GROUP BY k) q',False)
 result={'mysql_version':version,'image':image,'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),'proof_binary_sha256':hashlib.sha256((root/'experiments/core-engine-100/target/release/proof').read_bytes()).hexdigest(),'checks':checks,'count':len(checks),'witnesses':witnesses,'scope':'336 decoded-update SQL comparisons; counterexamples are MySQL semantic guards, not a shipped optimizer rule'}
 (out/'result.json').write_text(json.dumps(result,indent=2)+'\n')
 (out/'artifact-sha256.json').write_text(json.dumps({str(f.relative_to(out)):hashlib.sha256(f.read_bytes()).hexdigest() for f in sorted(out.rglob('phase-*')) if f.is_file()},indent=2)+'\n')
 print(f'F2-ORACLE-DONE comparisons={len(checks)} witnesses={len(witnesses)}',flush=True)
finally:
 if owned:docker(['rm','-f',name])
