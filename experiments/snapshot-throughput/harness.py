"""Snapshot throughput matrix over the synthetic source the crate's
`throughput` example builds. The A/B switches the experiment branch measured
are gone: the task-parallel workers and the expanded composite-key seek are
the only behaviour now, so this runs each case three times and records the
medians. Set SNAPSHOT_BENCH_DSN and SNAPSHOT_BENCH_OUTPUT; build the example
with `cargo build --release -p pintail-snapshot --example throughput` first.
"""
import json, os, pathlib, shutil, statistics, subprocess

root = pathlib.Path(__file__).resolve().parents[2]
out = pathlib.Path(os.environ["SNAPSHOT_BENCH_OUTPUT"]).resolve()
out.mkdir(parents=True, exist_ok=True)
binary = root / "target/release/examples/throughput"
# (label, example mode, workers, chunk rows)
cases = [
    ("single", "single", 4, 100_000),
    ("multi", "multi", 4, 100_000),
    ("ranges", "ranges", 4, 100_000),
    ("composite", "composite", 1, 10_000),
]
results = []
for repeat in range(3):
    ordered = cases if repeat % 2 == 0 else cases[::-1]
    for label, mode, workers, chunk in ordered:
        name = f"matrix-{label}-{repeat}"
        dest = out / name
        env = os.environ.copy()
        env.update(SNAPSHOT_BENCH_DSN=os.environ["SNAPSHOT_BENCH_DSN"], SNAPSHOT_BENCH_CHUNK_ROWS=str(chunk))
        with (out / (name + ".log")).open("w") as log:
            subprocess.run([str(binary), mode, str(workers), str(dest)], env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
        result = next(json.loads(line) for line in (out / (name + ".log")).read_text().splitlines() if line.startswith("{"))
        assert result["verified"] and result["rows"] == 1_000_000
        result.update(case=label, repeat=repeat, chunk_rows=chunk)
        results.append(result)
        (out / "matrix-results.json").write_text(json.dumps(results, indent=2) + "\n")
        shutil.rmtree(dest)
summary = {label: statistics.median(x["seconds"] for x in results if x["case"] == label) for label, *_ in cases}
(out / "matrix-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print("EXPERIMENT-DONE", json.dumps(summary), flush=True)
