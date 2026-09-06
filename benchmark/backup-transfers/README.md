# S3 backup transfer experiment

Date: 2026-09-06. Baseline: `fdd7222`; candidate: `ffc5858`.

Streaming with four concurrent objects substantially improved full-backup and restore throughput in this local S3 experiment. Upload memory is bounded by concurrency and part size, but can exceed the current serial implementation for small and medium segments. Incremental speedups were smaller and not universal.

## Four concurrent objects versus baseline

Times and peak client RSS below are medians of three fresh-process trials after one warm-up. RSS includes the Rust process, not the S3 service. Speedup is baseline time divided by candidate time.

| Synthetic shape | Operation | Baseline s | Candidate s | Speedup | Baseline MiB | Candidate MiB |
|---|---|---:|---:|---:|---:|---:|
| 64 × 1 MiB | full | 0.295 | 0.106 | 2.78× | 9.1 | 22.1 |
| 64 × 1 MiB | incremental | 0.095 | 0.113 | 0.84× | 9.1 | 17.4 |
| 64 × 1 MiB | restore | 0.081 | 0.057 | 1.42× | 10.7 | 21.9 |
| 16 × 64 MiB | full | 3.429 | 1.065 | 3.22× | 71.9 | 87.5 |
| 16 × 64 MiB | incremental | 1.347 | 0.959 | 1.40× | 71.7 | 32.5 |
| 16 × 64 MiB | restore | 0.887 | 0.459 | 1.93× | 74.6 | 20.6 |
| 4 × 256 MiB | full | 3.242 | 1.116 | 2.91× | 264.0 | 79.5 |
| 4 × 256 MiB | incremental | 1.345 | 1.232 | 1.09× | 264.0 | 25.6 |
| 4 × 256 MiB | restore | 0.898 | 0.453 | 1.98× | 265.3 | 20.9 |

## Interpretation

- Four concurrent objects is a useful starting point, not a proven optimum. At 64 MiB, eight concurrent uploads used about 152 MiB versus 88 MiB at four, with essentially the same full-backup median.
- Streaming one object at a time brought large-segment full-backup RSS down to about 25 MiB while still improving full-backup time. Choose a lower concurrency when memory matters more than maximum throughput.
- Four-way upload RSS was higher than baseline for the 1 MiB and 64 MiB shapes. The baseline only buffers one whole segment; concurrent streaming has its own per-object buffers.
- Small incremental backups regressed from about 95 ms to 113 ms at concurrency four. Do not claim every operation benefits.
- The large shape has only four segments, so settings four and eight have the same effective object concurrency. Their differing upload times show run-to-run variability; the results do not establish that the higher setting is inherently slower.

## Method and scope

Code was authored in an isolated Mac worktree, copied to a Linux x86-64 machine with 32 logical CPUs, and compiled there using Rust 1.97.1. Both binaries used the same lockfile, release profile, allocator and four-thread Tokio runtime. No release-profile overrides or native CPU flags were used.

A dedicated MinIO process served a new temporary bucket over loopback HTTP, with source files, object storage and restored files on local NVMe. The service and its data were removed after the run. This measures Pintail’s backup transport, hashing and local I/O; it does not represent WAN latency, AWS S3 throttling, TLS overhead, production segment distributions, CDC snapshot capture, or query contention.

Payloads were synthetic byte files (not valid engine segments), built from repeated pseudorandom 1 MiB blocks. Neither implementation compressed them. Full backups contained 64 MiB or 1 GiB. Incremental backups changed one in four segment payloads while retaining their logical names, forcing checksum-based reuse decisions. Restores used the incremental manifest and checked every object’s size and SHA-256. A separate Python file comparison checked restored bytes against source hashes outside the timed region.

Each shape/configuration ran one warm-up plus three measured full → incremental → restore chains. Configuration order rotated between repetitions. Filesystem caches were left warm; global cache drops were not used. Each operation ran in a fresh process under GNU time. The `seconds` field times the library operation including S3 manifest loading where needed; `wall_seconds` also includes process startup. RSS is GNU time’s process maximum. Warm-ups remain in the raw artifact and are excluded from tables.

## Candidate behavior

- Retains the existing S3 client and backup manifest format.
- Uses one PUT for segments up to 8 MiB; larger segments use 8 MiB multipart chunks with at most two part requests in flight per object.
- Hashes full-backup segments while uploading, avoiding the baseline’s duplicate full-backup SHA-256 calculation.
- Streams source hashing for incremental reuse checks. Changed inherited segments are read again for upload.
- Streams restored objects through SHA-256 into 1 MiB buffered file writers.
- Preserves segment order, publishes the backup manifest last, and drains active transfers before reporting errors. Multipart failures attempt abort; restore failures remove staging. Duplicate restore paths are rejected before concurrent writes start.
- Uses four concurrent objects by default, with explicit library options from one to 32. This is an experimental branch; the main checkout is unchanged.

The explicit error paths were checked, but forced process termination, multipart abort failures, retry fault injection and cloud-provider behavior need separate testing before treating this as release evidence.

## Correctness

All 144 warm-up/measured operations completed, all 48 independent restored-file comparisons passed, and mixed baseline/candidate backup chains restored successfully in both directions. Five backup integration tests passed, including multipart tails, incremental reuse, same-size corruption rejection, failed backup publication, and duplicate restore-path rejection.

## All configurations

| Shape | Variant | Objects | Operation | Min s | Median s | Max s | Median peak MiB | Max peak MiB |
|---|---|---:|---|---:|---:|---:|---:|---:|
| large | baseline | 1 | full | 3.241 | 3.242 | 3.678 | 264.0 | 264.1 |
| large | baseline | 1 | incremental | 1.337 | 1.345 | 1.618 | 264.0 | 264.1 |
| large | baseline | 1 | restore | 0.892 | 0.898 | 0.904 | 265.3 | 266.8 |
| large | streaming | 1 | full | 1.808 | 1.878 | 2.039 | 25.4 | 25.7 |
| large | streaming | 1 | incremental | 0.927 | 1.107 | 1.142 | 25.2 | 25.7 |
| large | streaming | 1 | restore | 0.454 | 0.454 | 0.456 | 11.7 | 11.8 |
| large | streaming | 4 | full | 1.083 | 1.116 | 1.764 | 79.5 | 79.8 |
| large | streaming | 4 | incremental | 0.901 | 1.232 | 1.257 | 25.6 | 25.7 |
| large | streaming | 4 | restore | 0.453 | 0.453 | 0.455 | 20.9 | 23.3 |
| large | streaming | 8 | full | 1.585 | 1.631 | 1.879 | 79.1 | 79.1 |
| large | streaming | 8 | incremental | 1.236 | 1.277 | 1.323 | 25.3 | 25.5 |
| large | streaming | 8 | restore | 0.450 | 0.451 | 0.453 | 21.5 | 22.0 |
| medium | baseline | 1 | full | 3.401 | 3.429 | 3.472 | 71.9 | 72.1 |
| medium | baseline | 1 | incremental | 1.344 | 1.347 | 1.370 | 71.7 | 72.1 |
| medium | baseline | 1 | restore | 0.884 | 0.887 | 0.899 | 74.6 | 75.1 |
| medium | streaming | 1 | full | 1.957 | 1.976 | 1.994 | 26.1 | 26.3 |
| medium | streaming | 1 | incremental | 1.029 | 1.034 | 1.044 | 25.8 | 26.1 |
| medium | streaming | 1 | restore | 0.469 | 0.472 | 0.472 | 12.8 | 12.9 |
| medium | streaming | 4 | full | 1.065 | 1.065 | 1.089 | 87.5 | 91.7 |
| medium | streaming | 4 | incremental | 0.955 | 0.959 | 0.966 | 32.5 | 33.0 |
| medium | streaming | 4 | restore | 0.458 | 0.459 | 0.468 | 20.6 | 22.6 |
| medium | streaming | 8 | full | 1.057 | 1.060 | 1.062 | 152.4 | 152.6 |
| medium | streaming | 8 | incremental | 0.748 | 0.751 | 0.762 | 50.1 | 50.7 |
| medium | streaming | 8 | restore | 0.458 | 0.460 | 0.461 | 29.5 | 30.6 |
| small | baseline | 1 | full | 0.283 | 0.295 | 0.295 | 9.1 | 9.5 |
| small | baseline | 1 | incremental | 0.093 | 0.095 | 0.101 | 9.1 | 9.2 |
| small | baseline | 1 | restore | 0.081 | 0.081 | 0.085 | 10.7 | 11.8 |
| small | streaming | 1 | full | 0.261 | 0.275 | 0.275 | 10.1 | 10.4 |
| small | streaming | 1 | incremental | 0.099 | 0.101 | 0.108 | 10.1 | 10.2 |
| small | streaming | 1 | restore | 0.105 | 0.111 | 0.115 | 12.2 | 12.5 |
| small | streaming | 4 | full | 0.100 | 0.106 | 0.108 | 22.1 | 25.8 |
| small | streaming | 4 | incremental | 0.111 | 0.113 | 0.115 | 17.4 | 17.4 |
| small | streaming | 4 | restore | 0.056 | 0.057 | 0.067 | 21.9 | 22.4 |
| small | streaming | 8 | full | 0.093 | 0.095 | 0.095 | 37.5 | 37.5 |
| small | streaming | 8 | incremental | 0.067 | 0.072 | 0.072 | 24.9 | 25.9 |
| small | streaming | 8 | restore | 0.061 | 0.063 | 0.065 | 28.3 | 29.4 |

## Reproduce

Build the baseline example at `fdd7222`, save its release executable as `EXPERIMENT_ROOT/bin/baseline`, then build the candidate at `ffc5858` and save it as `EXPERIMENT_ROOT/bin/streaming`. Put the pinned service binary named in `metadata.json` at `EXPERIMENT_ROOT/bin/minio`. Use the same toolchain for both builds. Ensure ports 39091 and 39092 are free and allow roughly 12 GiB for temporary benchmark data plus build artifacts.

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --locked --release -p pintail-backup --example s3_transfer_bench
python3 scripts/bench-backup-transfers.py EXPERIMENT_ROOT
```

The runner generates fresh credentials, starts its own S3 service, records binary hashes, alternates configurations, verifies results and removes its temporary data. Raw measurements are in [measurements.jsonl](measurements.jsonl); toolchain, binary hashes and scope are in [metadata.json](metadata.json).
