# S3 backup transfer experiment

Date: 2026-09-06. Baseline: `fdd7222`; candidate: `7cf219e`.

Streaming with four concurrent objects substantially improved full-backup and restore throughput in this local S3 experiment. Upload memory is bounded by concurrency and part size, but can exceed the current serial implementation for small and medium segments. Incremental results varied by shape and concurrency.

## Four concurrent objects versus baseline

Times and peak client RSS below are medians of three fresh-process trials after one warm-up. RSS includes the Rust process, not the S3 service. Speedup is baseline time divided by candidate time.

| Synthetic shape | Operation | Baseline s | Candidate s | Speedup | Baseline MiB | Candidate MiB |
|---|---|---:|---:|---:|---:|---:|
| 64 × 1 MiB | full | 0.290 | 0.099 | 2.92× | 9.0 | 22.6 |
| 64 × 1 MiB | incremental | 0.101 | 0.110 | 0.92× | 9.1 | 16.8 |
| 64 × 1 MiB | restore | 0.094 | 0.067 | 1.40× | 11.6 | 21.5 |
| 16 × 64 MiB | full | 3.514 | 1.067 | 3.29× | 71.9 | 88.4 |
| 16 × 64 MiB | incremental | 1.412 | 0.968 | 1.46× | 71.8 | 32.6 |
| 16 × 64 MiB | restore | 0.913 | 0.459 | 1.99× | 74.4 | 21.1 |
| 4 × 256 MiB | full | 3.259 | 1.599 | 2.04× | 264.1 | 80.2 |
| 4 × 256 MiB | incremental | 1.355 | 1.248 | 1.09× | 264.1 | 25.9 |
| 4 × 256 MiB | restore | 0.876 | 0.451 | 1.94× | 265.9 | 23.0 |

## Interpretation

- Four concurrent objects is a useful starting point, not a proven optimum. At 64 MiB, eight concurrent uploads used 152 MiB versus 88 MiB at four; full-backup medians were 1.065 s and 1.067 s respectively.
- Streaming one object at a time brought large-segment full-backup RSS down to 26 MiB while still improving full-backup time. Choose a lower concurrency when memory matters more than maximum throughput.
- Four-way upload RSS was higher than baseline for the 1 MiB and 64 MiB shapes. The baseline only buffers one whole segment; concurrent streaming has its own per-object buffers.
- Small incremental backup medians were 101 ms at baseline and 110 ms at concurrency four. Incremental results are sensitive to segment count and the work required to check unchanged data.
- The large shape has only four segments, so settings four and eight have the same effective object concurrency. Differences between their results reflect run-to-run variability; they do not establish an inherent advantage for either setting.

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

The benchmark was rerun after fixing transfer-future compatibility with multithreaded HTTP handlers. The artifacts here describe that corrected binary.

All 144 warm-up/measured operations completed, all 48 independent restored-file comparisons passed, and mixed baseline/candidate backup chains restored successfully in both directions. Six backup integration tests passed, including multipart tails, incremental reuse, same-size corruption rejection, failed backup publication, duplicate restore-path rejection, and Send futures for multithreaded HTTP handlers.

## All configurations

| Shape | Variant | Objects | Operation | Min s | Median s | Max s | Median peak MiB | Max peak MiB |
|---|---|---:|---|---:|---:|---:|---:|---:|
| large | baseline | 1 | full | 3.201 | 3.259 | 3.633 | 264.1 | 264.5 |
| large | baseline | 1 | incremental | 1.321 | 1.355 | 1.448 | 264.1 | 264.2 |
| large | baseline | 1 | restore | 0.869 | 0.876 | 0.880 | 265.9 | 266.3 |
| large | streaming | 1 | full | 1.893 | 1.945 | 3.403 | 26.0 | 26.2 |
| large | streaming | 1 | incremental | 0.984 | 1.044 | 1.327 | 26.1 | 26.5 |
| large | streaming | 1 | restore | 0.454 | 0.454 | 0.456 | 12.3 | 13.0 |
| large | streaming | 4 | full | 1.383 | 1.599 | 1.812 | 80.2 | 80.2 |
| large | streaming | 4 | incremental | 1.234 | 1.248 | 1.315 | 25.9 | 26.3 |
| large | streaming | 4 | restore | 0.451 | 0.451 | 0.452 | 23.0 | 23.2 |
| large | streaming | 8 | full | 2.208 | 2.266 | 2.437 | 80.3 | 80.6 |
| large | streaming | 8 | incremental | 0.956 | 1.240 | 1.254 | 25.9 | 26.3 |
| large | streaming | 8 | restore | 0.450 | 0.451 | 0.452 | 22.2 | 23.8 |
| medium | baseline | 1 | full | 3.409 | 3.514 | 4.306 | 71.9 | 71.9 |
| medium | baseline | 1 | incremental | 1.376 | 1.412 | 1.459 | 71.8 | 72.4 |
| medium | baseline | 1 | restore | 0.902 | 0.913 | 0.914 | 74.4 | 74.6 |
| medium | streaming | 1 | full | 1.961 | 2.379 | 2.613 | 26.5 | 26.6 |
| medium | streaming | 1 | incremental | 1.025 | 1.329 | 1.342 | 26.1 | 26.3 |
| medium | streaming | 1 | restore | 0.469 | 0.472 | 0.473 | 12.5 | 12.8 |
| medium | streaming | 4 | full | 1.065 | 1.067 | 2.257 | 88.4 | 91.7 |
| medium | streaming | 4 | incremental | 0.956 | 0.968 | 1.291 | 32.6 | 34.7 |
| medium | streaming | 4 | restore | 0.456 | 0.459 | 0.462 | 21.1 | 21.5 |
| medium | streaming | 8 | full | 1.063 | 1.065 | 1.854 | 152.3 | 152.7 |
| medium | streaming | 8 | incremental | 0.754 | 0.756 | 1.158 | 50.4 | 51.3 |
| medium | streaming | 8 | restore | 0.459 | 0.461 | 0.462 | 30.2 | 33.0 |
| small | baseline | 1 | full | 0.289 | 0.290 | 0.306 | 9.0 | 9.0 |
| small | baseline | 1 | incremental | 0.095 | 0.101 | 0.102 | 9.1 | 9.1 |
| small | baseline | 1 | restore | 0.085 | 0.094 | 0.097 | 11.6 | 12.4 |
| small | streaming | 1 | full | 0.280 | 0.282 | 0.295 | 10.3 | 10.4 |
| small | streaming | 1 | incremental | 0.100 | 0.101 | 0.105 | 10.0 | 10.5 |
| small | streaming | 1 | restore | 0.101 | 0.110 | 0.117 | 12.9 | 13.7 |
| small | streaming | 4 | full | 0.096 | 0.099 | 0.104 | 22.6 | 22.7 |
| small | streaming | 4 | incremental | 0.110 | 0.110 | 0.118 | 16.8 | 18.0 |
| small | streaming | 4 | restore | 0.065 | 0.067 | 0.069 | 21.5 | 22.3 |
| small | streaming | 8 | full | 0.091 | 0.092 | 0.093 | 37.9 | 38.1 |
| small | streaming | 8 | incremental | 0.065 | 0.070 | 0.073 | 25.3 | 28.0 |
| small | streaming | 8 | restore | 0.051 | 0.057 | 0.060 | 27.2 | 30.1 |

## Reproduce

Build the baseline example at `fdd7222`, save its release executable as `EXPERIMENT_ROOT/bin/baseline`, then build the candidate at `7cf219e` and save it as `EXPERIMENT_ROOT/bin/streaming`. Put the pinned service binary named in `metadata.json` at `EXPERIMENT_ROOT/bin/minio`. Use the same toolchain for both builds. Ensure ports 39091 and 39092 are free and allow roughly 12 GiB for temporary benchmark data plus build artifacts.

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --locked --release -p pintail-backup --example s3_transfer_bench
python3 scripts/bench-backup-transfers.py EXPERIMENT_ROOT
```

The runner generates fresh credentials, starts its own S3 service, records binary hashes, alternates configurations, verifies results and removes its temporary data. Raw measurements are in [measurements.jsonl](measurements.jsonl); toolchain, binary hashes and scope are in [metadata.json](metadata.json).
