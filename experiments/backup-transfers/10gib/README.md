# 10 GiB S3 backup transfer experiment

Date: 2026-09-06. Each dataset is 10 GiB (10,737,418,240 payload bytes).

The same baseline and candidate binaries from the earlier experiment were reused; their SHA-256 hashes match exactly. The engine implementation did not change. The runner adds a disk-space preflight, larger datasets and cleanup between shapes.

## Four concurrent objects versus baseline

Times and peak client RSS are medians of three fresh-process trials after one warm-up. Client RSS excludes the S3 service. Full/restore throughput uses 10 GiB; incremental uploads carry 2.5 GiB plus metadata while checking the full source dataset.

| Synthetic shape | Operation | Baseline s | Streaming s | Speedup | Baseline peak MiB | Streaming peak MiB |
|---|---|---:|---:|---:|---:|---:|
| 160 × 64 MiB | full | 47.744 | 26.080 | 1.83× | 72.0 | 107.1 |
| 160 × 64 MiB | incremental | 17.068 | 13.470 | 1.27× | 71.7 | 39.1 |
| 160 × 64 MiB | restore | 22.354 | 13.600 | 1.64× | 74.5 | 25.6 |
| 40 × 256 MiB | full | 42.905 | 26.774 | 1.60× | 263.9 | 97.0 |
| 40 × 256 MiB | incremental | 14.708 | 13.620 | 1.08× | 263.9 | 32.5 |
| 40 × 256 MiB | restore | 20.202 | 10.788 | 1.87× | 266.5 | 28.5 |

## Interpretation

Four concurrent objects remains the best measured full-backup setting in this experiment. It had the fastest median in both layouts while using substantially less memory than eight concurrent objects.

- At 256 MiB per segment, four-way streaming reduced upload peak RSS from 264 MiB to 97 MiB and restore peak RSS from 267 MiB to 29 MiB.
- At 64 MiB per segment, upload RSS increased from 72 MiB to 107 MiB. Restore RSS decreased from 75 MiB to 26 MiB. Concurrency is a memory tradeoff, even though each transfer is streamed.
- Serial streaming kept upload RSS near 25–26 MiB and restore RSS near 12 MiB, but full backups took 36–40 seconds rather than 26–27 seconds with four concurrent objects.
- Eight concurrent objects had the fastest incremental medians (10.9–11.5 seconds), but slower full-backup medians (28.7–30.6 seconds) and higher upload RSS (175–177 MiB). More concurrency did not improve every operation.
- The earlier 1 GiB cases showed full-backup speedups of 3.29× and 2.04× for four-way streaming. At 10 GiB those became 1.83× and 1.60×. The smaller experiment overstates the relative throughput benefit at this larger working set.

The three-trial ranges below show material variability in some operations, especially restores. These results support four as a useful default for this setup, not a universal optimum for cloud storage.

## Scope and method

Two layouts hold the same total payload: 160 files of 64 MiB, and 40 files of 256 MiB. All four configurations now have enough files to use their requested object concurrency, including eight concurrent objects for the larger segment size.

The baseline is serial, whole-object I/O. Streaming configurations use one, four and eight concurrent objects, 8 MiB multipart chunks, at most two in-flight parts per object, and 1 MiB buffered restore writers. All binaries use four Tokio runtime workers and the default system allocator.

A dedicated temporary MinIO bucket ran over loopback HTTP. Source files, S3 objects and restored files were on local NVMe. Synthetic payload generation, independent checksum comparison and cleanup were outside the timed operations. No compression was performed. Each shape used one warm-up plus three measured rounds, rotating configuration order between rounds. Every operation ran in a fresh process under GNU time.

Caches were not flushed. Unlike the earlier smaller datasets, the complete source/object/restore working set is about 35 GiB and can exceed available page cache. These results include local filesystem and cache effects; they do not measure cloud S3 latency, TLS, source snapshot capture, concurrent queries, or real engine segment decoding.

The incremental case changes one in four payloads under the same logical names. Each restore checks sizes and SHA-256 against the incremental manifest, followed by independent comparison with source hashes. The runner also checks mixed-version backup chains in both directions. All 96 operations and 32 independently verified restores passed, covering 3,200 segment-file comparisons plus the separate compatibility checks. Temporary S3 data and source files were removed after the run.

## All configurations

| Shape | Variant | Objects | Operation | Min s | Median s | Max s | Median peak MiB | Max peak MiB |
|---|---|---:|---|---:|---:|---:|---:|---:|
| large-10gib | baseline | 1 | full | 36.640 | 42.905 | 43.030 | 263.9 | 264.0 |
| large-10gib | baseline | 1 | incremental | 14.392 | 14.708 | 14.930 | 263.9 | 264.0 |
| large-10gib | baseline | 1 | restore | 19.563 | 20.202 | 21.326 | 266.5 | 267.6 |
| large-10gib | streaming | 1 | full | 35.981 | 40.349 | 42.772 | 25.6 | 25.6 |
| large-10gib | streaming | 1 | incremental | 13.393 | 14.184 | 14.236 | 25.2 | 25.5 |
| large-10gib | streaming | 1 | restore | 9.161 | 18.043 | 22.931 | 11.8 | 12.3 |
| large-10gib | streaming | 4 | full | 26.225 | 26.774 | 27.781 | 97.0 | 109.0 |
| large-10gib | streaming | 4 | incremental | 12.410 | 13.620 | 13.918 | 32.5 | 32.5 |
| large-10gib | streaming | 4 | restore | 10.450 | 10.788 | 14.124 | 28.5 | 30.9 |
| large-10gib | streaming | 8 | full | 29.759 | 30.602 | 32.388 | 175.0 | 179.2 |
| large-10gib | streaming | 8 | incremental | 11.413 | 11.480 | 12.160 | 69.8 | 71.5 |
| large-10gib | streaming | 8 | restore | 8.600 | 13.126 | 13.208 | 44.2 | 46.9 |
| medium-10gib | baseline | 1 | full | 46.679 | 47.744 | 47.908 | 72.0 | 72.0 |
| medium-10gib | baseline | 1 | incremental | 16.679 | 17.068 | 17.289 | 71.7 | 72.0 |
| medium-10gib | baseline | 1 | restore | 22.349 | 22.354 | 22.729 | 74.5 | 75.9 |
| medium-10gib | streaming | 1 | full | 34.752 | 36.022 | 45.015 | 25.4 | 25.8 |
| medium-10gib | streaming | 1 | incremental | 14.877 | 15.218 | 15.689 | 33.1 | 33.3 |
| medium-10gib | streaming | 1 | restore | 18.316 | 19.794 | 20.188 | 12.5 | 12.7 |
| medium-10gib | streaming | 4 | full | 23.649 | 26.080 | 28.121 | 107.1 | 107.2 |
| medium-10gib | streaming | 4 | incremental | 12.773 | 13.470 | 14.762 | 39.1 | 45.4 |
| medium-10gib | streaming | 4 | restore | 12.039 | 13.600 | 14.126 | 25.6 | 29.1 |
| medium-10gib | streaming | 8 | full | 28.148 | 28.724 | 30.911 | 177.3 | 178.7 |
| medium-10gib | streaming | 8 | incremental | 10.707 | 10.860 | 12.347 | 78.1 | 81.8 |
| medium-10gib | streaming | 8 | restore | 11.253 | 11.788 | 13.865 | 41.0 | 41.6 |

## Reproduce

Use the binary setup in the [original report](../README.md), with at least 38 GiB free in the experiment directory before starting.

```sh
python3 experiments/backup-transfers/harness.py EXPERIMENT_ROOT --dataset 10gib
```

[Raw measurements](measurements.jsonl) · [Binary hashes and metadata](metadata.json)

## Development validation

The complete `development` profile passed on clean commit `dda1f7470` after
benchmark cleanup: formatting, workspace Clippy with warnings denied,
dashboard typechecking, and 893 unit tests passed; 25 normally ignored tests
were skipped. Validation used the pinned Rust 1.97.0. The transport benchmarks
reused the matched Rust 1.97.1 binaries from the earlier experiment.
See [validation.json](validation.json). This is not a release gate.
