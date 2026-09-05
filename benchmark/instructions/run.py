#!/usr/bin/env python3
"""Count executor instructions; reject incompatible baselines and changed answers."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile

CASES = ("filter", "aggregate", "join", "sort")
ROOT = Path(__file__).resolve().parents[2]


def output(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def compare(baseline, current, threshold):
    for key in ("toolchain", "valgrind", "architecture", "workload_sha256", "threads"):
        if baseline[key] != current[key]:
            raise ValueError(f"incompatible baseline: {key}")
    if set(baseline["instructions"]) != set(CASES):
        raise ValueError("baseline must contain every workload")
    regressions = {}
    for case in CASES:
        before, after = baseline["instructions"][case], current["instructions"][case]
        if not isinstance(before, (int, float)) or before <= 0:
            raise ValueError(f"invalid baseline count: {case}")
        percent = (after / before - 1) * 100
        if percent > threshold:
            regressions[case] = round(percent, 3)
    return regressions


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--baseline", type=Path, default=ROOT / "benchmark/instructions/baseline-linux.json")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--record", action="store_true", help="record evidence without claiming a comparison pass")
    parser.add_argument("--threshold", type=float, default=5.0)
    parser.add_argument("--label", default="release")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("Callgrind measurement requires Linux; use the provided Dockerfile")
    if not 0 <= args.threshold <= 100:
        parser.error("threshold must be finite and between 0 and 100")
    binary = args.binary.resolve(strict=True)
    result = {
        "schema": 1,
        "head": (ROOT / "source-commit").read_text().strip() if (ROOT / "source-commit").exists() else output("git", "rev-parse", "HEAD"),
        "label": args.label,
        "toolchain": output("rustc", "-vV"),
        "valgrind": output("valgrind", "--version"),
        "architecture": platform.machine(),
        "threads": 1,
        "workload_sha256": hashlib.sha256((ROOT / "crates/pintail-exec/examples/instruction_workload.rs").read_bytes()).hexdigest(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "instructions": {},
        "samples": {},
    }
    env = {**os.environ, "RAYON_NUM_THREADS": "1"}
    with tempfile.TemporaryDirectory(prefix="pintail-instructions-") as directory:
        for case in CASES:
            samples = []
            for repeat in range(3):
                profile = Path(directory) / f"{case}-{repeat}.callgrind"
                run = subprocess.run([
                    "valgrind", "--tool=callgrind", "--quiet", "--error-exitcode=2",
                    "--collect-atstart=no", "--toggle-collect=*instruction_workload::measured*",
                    f"--callgrind-out-file={profile}", str(binary), case,
                ], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
                if f"{case}: OK rows=" not in run.stdout:
                    raise ValueError(f"missing correctness witness: {case}")
                text = profile.read_text()
                if "instruction_workload::measured" not in text:
                    raise ValueError("measurement boundary missing from Callgrind profile")
                counts = [int(line.split()[1]) for line in text.splitlines() if line.startswith("summary:")]
                if len(counts) != 1 or counts[0] <= 0:
                    raise ValueError(f"invalid instruction count: {case}")
                samples.append(counts[0])
            result["samples"][case] = samples
            result["instructions"][case] = statistics.median(samples)
    result["verdict"] = "RECORDED"
    if not args.record:
        result["regressions_percent"] = compare(json.loads(args.baseline.read_text()), result, args.threshold)
        result["threshold_percent"] = args.threshold
        result["verdict"] = "FAIL" if result["regressions_percent"] else "PASS"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 1 if result["verdict"] == "FAIL" else 0


if __name__ == "__main__":
    raise SystemExit(main())
