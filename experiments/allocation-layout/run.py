#!/usr/bin/env python3
"""Run isolated processes in a seeded shuffled order; retain every reading."""
import hashlib
import itertools
import json
import os
from pathlib import Path
import random
import statistics
import subprocess

root = Path(__file__).resolve().parent
out = root / "evidence"
out.mkdir(exist_ok=True)
binary = root / "target/release/allocation-layout"
cpus = sorted(os.sched_getaffinity(0))[:4]
cases = [("narrow_numeric", 4, 0, 0), ("wide_numeric", 16, 0, 0),
         ("mixed_short", 8, 25, 24), ("mostly_long_text", 8, 75, 192)]
jobs = list(itertools.product(range(3), cases, [1, 4], ["nested", "boxed", "flat", "compact", "arena"]))
random.Random(8107).shuffle(jobs)
meta = {"base_commit": (root / "BASE_COMMIT").read_text().strip(),
        "rustc": subprocess.check_output([str(Path.home() / ".cargo/bin/rustc"), "--version"], text=True).strip(),
        "machine": os.uname().machine, "logical_cpus": os.cpu_count(), "affinity_cpu_count": len(cpus),
        "allocator": "Rust default System allocator", "seed": 8107,
        "rows": 131072, "probes_per_batch": 262144, "process_repeats": 3,
        "timed_batches_per_process": 7, "source_sha256": hashlib.sha256((root / "src/main.rs").read_bytes()).hexdigest()}
(out / "environment.json").write_text(json.dumps(meta, indent=2) + "\n")
results = []
with (out / "raw.jsonl").open("w") as stream:
    for repeat, (case, width, percent, length), workers, arm in jobs:
        command = ["taskset", "-c", ",".join(map(str, cpus[:workers])), str(binary), arm,
                   "131072", str(width), str(percent), str(length), str(workers), "262144"]
        result = json.loads(subprocess.check_output(command, text=True))
        result.update(case=case, repeat=repeat)
        results.append(result)
        stream.write(json.dumps(result) + "\n")
        stream.flush()
summary = []
for case, _, _, _ in cases:
    for workers in [1, 4]:
        for arm in ["nested", "boxed", "flat", "compact", "arena"]:
            readings = [r for r in results if (r["case"], r["workers"], r["arm"]) == (case, workers, arm)]
            batches = sorted(t for r in readings for t in r["batch_ns"])
            summary.append(dict(case=case, workers=workers, arm=arm,
                retained_bytes=readings[0]["retained_bytes"], live_allocations=readings[0]["live_allocations"],
                build_ms=statistics.median(r["build_ns"] for r in readings)/1e6,
                materialize_ms=statistics.median(t for r in readings for t in r["materialize_ns"])/1e6,
                drop_ms=statistics.median(r["drop_ns"] for r in readings)/1e6,
                batch_median_ms=statistics.median(batches)/1e6, batch_min_ms=min(batches)/1e6,
                batch_p95_ms=batches[int(.95*(len(batches)-1))]/1e6,
                million_probes_sec=262144/statistics.median(batches)*1000,
                process_median_ms=[statistics.median(r["batch_ns"])/1e6 for r in readings],
                fixture_build_peak_kib=statistics.median(r["fixture_and_build_peak_kib"] for r in readings)))
        checksums = {r["checksum"] for r in results if r["case"] == case and r["workers"] == workers}
        assert len(checksums) == 1
(out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print("ALLOCATION-LAYOUT-DONE", flush=True)
