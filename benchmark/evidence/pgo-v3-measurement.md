# Portable PGO and x86-64-v3 PGO, measured (0a85254f)

This measures the two-binary proposal in `docs/release-builds.md` on the current
engine. (On these figures the proposal was withdrawn: the release ships the
portable profile-guided binary alone. See `docs/decisions.md`.) All three binaries were built from one commit on one machine: 8 vCPU,
16 GB, AMD Ryzen 9 9950X (`/proc/cpuinfo` was checked before every measurement
block), Ubuntu 24.04, rustc 1.97.0.

| arm | build | how |
|---|---|---|
| a | current release build | `cargo build --locked --release -p pintail` |
| f | same-commit floor | the same command again in a second, empty target directory |
| b | portable + PGO | `scripts/pgo-build.sh server` |
| c | x86-64-v3 + PGO | `PINTAIL_TARGET_CPU=x86-64-v3 scripts/pgo-build.sh server` |

## Build cost and size

| arm | cold build | binary (stripped) |
|---|---:|---:|
| a | 157 s | 270.4 MB (55.4 MB) |
| b | 549 s: instrumented 155 + training 232 + optimized 162 | 296.3 MB (59.6 MB) |
| c | 544 s: instrumented 149 + training 238 + optimized 157 | 295.7 MB (59.6 MB) |

Each build ran on its own with nothing else on the machine. The floor build
shared the machine with data loading, so its time is not given. `a` and `f` hold
the same code; only their debug information differs, as the target directory
names are part of it.

## Answers

Every binary gave the same answer as MySQL 8.4 on Q1-Q8 and on the eight
time-window statements, on every run (each run checked against MySQL's answer).
For small statements the answers matched across binaries, and three sampled key
lookups also matched MySQL. All 26 instruction-suite digests were the same. In
the corpus, every case-run was `ok` in all four binaries, every verdict against
MySQL was the same (836 equal and 4 not comparable for each binary), and there
were no differences.

## Server timings

**Method.** The 20M-row benchmark replica, plus a 10M-row event table: one row
per second from 2024-01-01, a `DATETIME` and a `TIMESTAMP` column, and the
instruction suite's eight time-window statements scaled up. The replica was
snapshotted natively once, and each arm read its own copy. Settings:
`PINTAIL_DISABLE_SETTLED_MEMO=1` and `PINTAIL_COMPACTION_INPUT_ROWS=1` on every
arm. Statements went over the MySQL wire, and the runs were interleaved: in each
round, all four servers started fresh, 2 warmups ran, then 15 cycles in which
each statement ran once on each arm in rotating order. A track had 6 rounds, and
each track ran three times; the tables pool all 18 rounds. A median is the
median over rounds of each round's median. CPU is the server process's
user+system time.

**The noise floor is arm `f`:** its gap from `a` is the noise.

### The eight time-window shapes (the open item in `docs/release-builds.md`)

| case | a median ms | f vs a | b vs a | c vs a | c vs b | c min vs a min |
|---|---:|---:|---:|---:|---:|---:|
| window_day | 6.5 | +0.0% | -1.8% | -3.3% | -1.5% | -3.3% |
| window_hour | 7.1 | +1.2% | -4.9% | -11.7% | -7.1% | -13.7% |
| zoned_window_day | 7.1 | +0.8% | -1.5% | -2.4% | -1.0% | -2.6% |
| zoned_window_formatted_day | 9.0 | +0.6% | -2.7% | -2.4% | +0.4% | -1.5% |
| zoned_window_hour | 8.7 | -0.8% | -2.5% | -4.7% | -2.2% | -4.0% |
| zoned_since_day | 7.5 | -0.4% | -1.6% | -2.2% | -0.6% | -2.7% |
| zoned_all_days | 24.1 | +0.4% | -1.7% | -2.0% | -0.3% | -3.6% |
| zoned_named_zone_hour | 39.7 | -0.9% | -21.8% | -21.7% | +0.2% | -21.5% |
| geomean | | +0.1% | -5.1% | -6.5% | -1.5% | |

CPU against `a`: f -0.5%, b -6.8%, c -7.7%. The three runs' geomeans were
b -5.3/-4.7/-5.3% and c -6.4/-6.2/-6.7%. Under v3 these shapes execute 1-11%
more instructions (below), but no shape is slower on the clock: `c` is level
with or ahead of `b` on all eight.

### Q1-Q8

| case | a median ms | f vs a | b vs a | c vs a | c vs b | c min vs a min |
|---|---:|---:|---:|---:|---:|---:|
| Q1 | 0.6 | +0.2% | -8.1% | -9.3% | -1.3% | -5.5% |
| Q2 | 2.7 | +0.2% | -3.9% | -7.6% | -3.9% | -9.9% |
| Q3 | 10.1 | +0.0% | -7.1% | -11.6% | -4.8% | -11.7% |
| Q4 | 13.7 | -0.8% | -1.3% | -1.4% | -0.1% | -0.7% |
| Q5 | 9.3 | -1.4% | -6.1% | -12.4% | -6.7% | -11.9% |
| Q6 | 22.5 | -3.6% | -9.8% | -9.9% | -0.1% | -8.7% |
| Q7 | 34.0 | -0.2% | -18.4% | -20.9% | -3.1% | -22.8% |
| Q8 | 14.0 | -1.6% | -6.7% | -5.7% | +1.0% | -7.6% |
| geomean | | -0.9% | -7.8% | -10.0% | -2.4% | |

CPU over Q2-Q8 against `a` (Q1 is below the 10 ms clock tick): f -0.9%,
b -8.8%, c -11.2%. The three runs' geomeans were f -0.5/-0.7/-0.8%,
b -9.6/-6.1/-7.1% and c -9.4/-10.0/-10.4%.

### Small statements, one wire connection (µs)

Per run: 12,000 executions per case per binary, over 3 rounds of fresh
processes, in blocks of 200 with the arms rotating.

| case | run | a median | f vs a | b vs a | c vs a |
|---|---:|---:|---:|---:|---:|
| `SELECT 1+1` | 1 / 2 / 3 | 16.3 / 16.3 / 17.5 | +3.4 / -0.3 / -1.3% | -1.8 / -2.3 / -3.4% | -2.3 / -2.3 / -3.1% |
| key lookup | 1 / 2 / 3 | 187 / 199 / 185 | -1.2 / -8.6 / +0.3% | -16.0 / -14.3 / -10.5% | -13.6 / -15.8 / -13.2% |

## Corpus, scale 10,000

These are the oracle corpus families `calendar edges date_format columns` (33
cases), `session zone America/New_York` (44), `boundary conversion contexts`
(126) and `row constructor comparisons` (7). The harness was
`benchmark/run-corpus.ts --runs 5 --no-clickhouse`, with one image per binary:
the Dockerfile's runtime stage on a base whose glibc matches the build host. Each
binary ran four passes, in the orders a c b f / c a f b / b f a c / f b c a. The
figure per case is the median over the passes. "norm" divides each pass by the
MySQL time for that case in the same pass, which takes out the machine's speed
state.

| family | cases | a ms (geomean) | f vs a | b vs a | c vs a | f norm | b norm | c norm |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| boundary conversion contexts | 126 | 15.5 | +3.4% | -2.9% | +1.6% | -0.8% | -1.6% | -3.4% |
| calendar edges date_format columns | 33 | 54.8 | +2.6% | +5.1% | +4.0% | +1.2% | +4.1% | +3.7% |
| row constructor comparisons | 7 | 18.1 | -2.3% | -4.9% | -3.4% | -1.7% | -2.9% | +2.2% |
| session zone America/New_York | 44 | 40.1 | -2.8% | -5.6% | -4.2% | -0.7% | -0.6% | -6.8% |
| all | 210 | 23.2 | +1.7% | -2.3% | +0.6% | -0.5% | -0.6% | -2.9% |

The corpus harness runs one binary at a time, not interleaved. A single pass
moved by up to 13% (`f` pass 1), and over four passes the floor is still about
±3%. No family moves beyond that. `calendar edges` is about 4% slower under both
profile-guided builds, which is at the edge of the floor and not a finding.

## Instruction suite (deterministic)

The 26 cases were run on four builds: `a`; `b` built with `b`'s profile; `c`
built with `c`'s profile and v3; and v3 with no profile (`-Ctarget-cpu=x86-64-v3`).
The table shows instructions against `a`.

| case | b (PGO) | c (v3+PGO) | v3 alone |
|---|---:|---:|---:|
| q2_filtered_count | +6.9% | -19.2% | -19.4% |
| q3_group_status | +2.0% | -12.6% | -13.5% |
| q4_region_status | +6.6% | +0.8% | -5.6% |
| q6_top_spenders | +2.3% | -13.0% | -15.2% |
| n1/n2 (filtered count, group by region) | +1.5/+1.0% | -13.6/-15.1% | -14.9/-16.1% |
| text_filter_two_columns | +18.3% | +6.7% | -11.0% |
| many_groups | +10.1% | +9.6% | -1.3% |
| count_distinct | -6.4% | -5.5% | +1.2% |
| window_day / window_hour | +0.3 / -1.4% | +3.3 / +6.7% | +6.0 / +9.8% |
| zoned windows (six cases) | -2.7% to +1.6% | +0.7% to +7.1% | +4.5% to +10.5% |

The full tables, with L1/LL misses and estimated cycles, are in the run's
artifacts. The profile-guided builds execute more instructions on most shapes
yet take less time: the gain is code layout. The suite does not model that, so
it cannot rank PGO builds.

## Start-time CPU detection

These checks used an image with the generic binary (`a`), the v3 binary (`c`)
and `scripts/pintail-launch.sh` as `/usr/local/bin/pintail`, in docker on this
host:

| case | started | PID 1 | kernels |
|---|---|---|---|
| default | `build_target=x86-64-v3 build_variant=pgo` | the v3 binary (the launcher `exec`s) | `simd=avx2` |
| `PINTAIL_BINARY=generic` | `build_target=generic build_variant=standard` | the generic binary | `simd=avx2` |
| `PINTAIL_SIMD=off` | v3 binary | v3 | `simd=baseline` |
| `/proc/cpuinfo` bind-mounted without `avx2`/`bmi2` | `build_target=generic` | generic | `simd=avx2` (the binary reads CPUID, not the file) |
| same, plus `PINTAIL_BINARY=x86-64-v3` | v3 binary | v3 | `simd=avx2` |

On a CPU without v3 (user-mode emulation of a Nehalem model), the generic binary
starts and reports `cpu_features=sse4.2 simd=baseline`. The v3 binary dies at
once with SIGILL (exit 132) before it logs anything. With a Haswell model, both
start. The launcher is the only guard, and it trusts the flags the kernel
reports. A forced `PINTAIL_BINARY=x86-64-v3` on such a machine crashes at start
instead of falling back. The v3 binary has 261,569 instructions that use `ymm`
registers; the generic binary has 38,267 (its runtime-dispatched kernels).
