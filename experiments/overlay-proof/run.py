#!/usr/bin/env python3
"""Apply counters only for this experiment, then restore production source."""
import hashlib,json,os,subprocess
from pathlib import Path
root=Path(__file__).resolve().parents[2]; here=Path(__file__).resolve().parent
scan=root/'crates/pintail-store/src/store/scan.rs'; original=scan.read_text()
binary_source=root/'experiments/core-engine-100/src/bin/overlay_probe.rs'
assert not binary_source.exists()
patched=original
patched+='\nstatic EXP_OVERLAY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);\nstatic EXP_MERGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);\nimpl ProjectedScanStream {\n pub fn experiment_eligible(&self)->bool {self.overlay_key.is_some()}\n pub fn experiment_counts(reset:bool)->(usize,usize) {use std::sync::atomic::Ordering::Relaxed;if reset {(EXP_OVERLAY.swap(0,Relaxed),EXP_MERGE.swap(0,Relaxed))}else{(EXP_OVERLAY.load(Relaxed),EXP_MERGE.load(Relaxed))}}\n}\n'
needle='            ScanPart::Merge { segments, lo, hi } => {'
assert patched.count(needle)==1
patched=patched.replace(needle,needle+'\n EXP_MERGE.fetch_add(1,std::sync::atomic::Ordering::Relaxed);')
needle='        let memtable = self.overlay_span_rows(slice, key_ids.len())?;'
assert patched.count(needle)==1
patched=patched.replace(needle,'        EXP_OVERLAY.fetch_add(1,std::sync::atomic::Ordering::Relaxed);\n'+needle)
env=dict(os.environ,CARGO_TARGET_DIR='target',RAYON_NUM_THREADS='4',PINTAIL_SCAN_THREADS='4')
try:
 scan.write_text(patched);binary_source.write_text((here/'main.rs').read_text())
 lab=root/'experiments/core-engine-100'
 subprocess.run([str(Path.home()/'.cargo/bin/cargo'),'build','--release','--bin','overlay_probe'],cwd=lab,env=env,check=True)
 binary=lab/'target/release/overlay_probe'
 (here/'provenance.json').write_text(json.dumps({'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),'original_scan_sha256':hashlib.sha256(original.encode()).hexdigest(),'instrumented_scan_sha256':hashlib.sha256(patched.encode()).hexdigest(),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'fixture_sha256':hashlib.sha256((here/'main.rs').read_bytes()).hexdigest(),'scope':'updates and deletes applied through decoded CDC, finite dirty snapshot; not continuous concurrency'},indent=2)+'\n')
 with (here/'raw.jsonl').open('x') as output:subprocess.run(['taskset','-c','0-7',str(binary)],env=env,stdout=output,check=True)
 print('OVERLAY-PROOF-DONE',flush=True)
finally:
 scan.write_text(original);binary_source.unlink(missing_ok=True)
