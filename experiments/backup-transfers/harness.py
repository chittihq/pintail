#!/usr/bin/env python3
"""Benchmark two prebuilt backup binaries against an isolated local S3 service.

Usage: python3 experiments/backup-transfers/harness.py EXPERIMENT_ROOT [--dataset 10gib]
Requires bin/{minio,baseline,streaming}, curl and GNU time. All service state
and synthetic data are temporary; only measurements and logs are retained.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import secrets
import shutil
import subprocess
import tempfile
import time
import urllib.request


def digest(path):
    with open(path, "rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("experiment_root", type=Path)
    parser.add_argument("--dataset", choices=["original", "10gib"], default="original")
    options = parser.parse_args()
    root = options.experiment_root.resolve()
    shapes = (
        [("medium-10gib", 160, 64), ("large-10gib", 40, 256)]
        if options.dataset == "10gib"
        else [("small", 64, 1), ("medium", 16, 64), ("large", 4, 256)]
    )
    # Source plus changed files, full plus incremental S3 objects, and one
    # restored copy coexist. Leave room for metadata and multipart staging.
    required = max((3 * count + 2 * ((count + 3) // 4)) * mib * 1024**2
                   for _, count, mib in shapes) + 3 * 1024**3
    if shutil.disk_usage(root).free < required:
        raise RuntimeError(f"benchmark needs at least {required / 1024**3:.1f} GiB free")
    run = root / "results" / time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    run.mkdir(parents=True)
    binaries = {name: root / "bin" / name for name in ["baseline", "streaming"]}
    metadata = {
        "architecture": platform.machine(),
        "logical_cpus": os.cpu_count(),
        "binary_sha256": {name: digest(path) for name, path in binaries.items()},
        "s3_service": subprocess.check_output([root / "bin/minio", "--version"], text=True),
        "transport": "loopback HTTP; local NVMe; no injected latency",
        "cache_policy": "no cache flushes; large working sets can exceed available page cache",
        "dataset": options.dataset,
        "shapes": [{"name": name, "segments": count, "segment_mib": mib}
                   for name, count, mib in shapes],
        "memory": "GNU time maximum client RSS; service memory excluded",
        "repetitions": 3,
        "warmups": 1,
        "incremental": "one in four segments changed; all source bytes checked",
        "data": "synthetic byte payloads; repeated pseudorandom 1 MiB blocks; no compression",
    }
    (run / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    endpoint = "http://127.0.0.1:39091"
    key, secret = "benchmark", secrets.token_hex(24)
    env = dict(os.environ, MINIO_ROOT_USER=key, MINIO_ROOT_PASSWORD=secret,
               BENCH_S3_ENDPOINT=endpoint, BENCH_S3_BUCKET="backup-benchmark",
               BENCH_S3_ACCESS_KEY=key, BENCH_S3_SECRET_KEY=secret)
    with tempfile.TemporaryDirectory(prefix="backup-transfer-data-", dir=root) as temporary:
        temporary = Path(temporary)
        service_log = open(run / "service.log", "w")
        service = subprocess.Popen(
            [root / "bin/minio", "server", temporary / "objects", "--address", "127.0.0.1:39091",
             "--console-address", "127.0.0.1:39092"],
            env=env, stdout=service_log, stderr=subprocess.STDOUT,
        )
        try:
            for _ in range(100):
                if service.poll() is not None:
                    raise RuntimeError("benchmark S3 service exited; see service.log")
                try:
                    with urllib.request.urlopen(endpoint + "/minio/health/ready", timeout=1):
                        break
                except OSError:
                    time.sleep(0.1)
            else:
                raise RuntimeError("benchmark S3 service did not become ready")
            subprocess.run([
                "curl", "--fail", "--silent", "--show-error", "--aws-sigv4", "aws:amz:us-east-1:s3",
                "--user", key + ":" + secret, "-X", "PUT", endpoint + "/backup-benchmark",
            ], check=True, capture_output=True)
            variants = [("baseline", 1), ("streaming", 1), ("streaming", 4), ("streaming", 8)]
            with open(run / "measurements.jsonl", "w") as measurements:
                for shape, count, mib in shapes:
                    source = temporary / shape
                    subprocess.run([binaries["baseline"], "prepare", source, str(count), str(mib), "prepare"], check=True)
                    expected = {
                        f"segment-{index:04}-base.pts": digest(source / f"segment-{index:04}-{'new' if index % 4 == 0 else 'base'}.pts")
                        for index in range(count)
                    }
                    for repeat in range(4):
                        order = variants[repeat:] + variants[:repeat]
                        for variant, concurrency in order:
                            prefix = f"{shape}-{repeat}-{variant}-{concurrency}"
                            args = [str(source), str(count), str(mib), prefix]
                            trial_env = dict(env, BENCH_CONCURRENCY=str(concurrency))
                            for operation in ["full", "incremental", "restore"]:
                                label = prefix + "-" + operation
                                metrics_path = run / (label + ".time.json")
                                with open(run / (label + ".log"), "w") as output:
                                    subprocess.run([
                                        "/usr/bin/time", "-o", str(metrics_path), "-f",
                                        '{"wall_seconds":%e,"max_rss_kib":%M,"user_seconds":%U,"system_seconds":%S}',
                                        str(binaries[variant]), operation, *args,
                                    ], env=trial_env, stdout=output, stderr=subprocess.STDOUT, check=True)
                                lines = (run / (label + ".log")).read_text().splitlines()
                                result = next(json.loads(line) for line in reversed(lines) if line.startswith("{"))
                                result.update(json.loads(metrics_path.read_text()))
                                result.update(shape=shape, variant=variant, concurrency=concurrency,
                                              repeat=repeat, warmup=(repeat == 0))
                                measurements.write(json.dumps(result) + "\n")
                                measurements.flush()
                            restored = source / ("restore-" + prefix) / "tables/table-records"
                            for name, expected_hash in expected.items():
                                if digest(restored / name) != expected_hash:
                                    raise RuntimeError("independent restored-file checksum mismatch")
                            subprocess.run([binaries[variant], "cleanup", *args], env=trial_env,
                                           stdout=subprocess.DEVNULL, check=True)
                    shutil.rmtree(source)
            # Exercise both directions with changed multipart objects and
            # references inherited from the other implementation's manifest.
            source = temporary / "interop"
            subprocess.run([binaries["baseline"], "prepare", source, "4", "17", "prepare"], check=True)
            expected = {
                f"segment-{index:04}-base.pts": digest(source / f"segment-{index:04}-{'new' if index % 4 == 0 else 'base'}.pts")
                for index in range(4)
            }
            for first, second in [("baseline", "streaming"), ("streaming", "baseline")]:
                prefix = "interop-" + first
                args = [str(source), "4", "17", prefix]
                with open(run / (prefix + ".log"), "w") as output:
                    for variant, operation in [(first, "full"), (second, "incremental"), (first, "restore")]:
                        subprocess.run([binaries[variant], operation, *args], env=env,
                                       stdout=output, stderr=subprocess.STDOUT, check=True)
                restored = source / ("restore-" + prefix) / "tables/table-records"
                for name, expected_hash in expected.items():
                    if digest(restored / name) != expected_hash:
                        raise RuntimeError("cross-version restore checksum mismatch")
                subprocess.run([binaries[first], "cleanup", *args], env=env, check=True)
            (run / "DONE").write_text("All operations, independent restored-file checks and bidirectional compatibility checks passed.\n")
            print("BACKUP-BENCH-DONE", run.name, flush=True)
        finally:
            service.terminate()
            try:
                service.wait(timeout=15)
            except subprocess.TimeoutExpired:
                service.kill()
                service.wait()
            service_log.close()


if __name__ == "__main__":
    main()
