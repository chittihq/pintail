# Release builds: what the build itself is worth

The published image holds one binary: a profile-guided build for the
platform's generic target (x86-64 or arm64), with thin LTO and one codegen
unit. This records what each build setting is worth on the current engine,
what a profile-guided build adds, and why the release ships that one
binary and not a second one compiled for a newer processor level. The last
section says how the image is built.

## How it was measured

One machine (8 virtual CPUs, 16 GB, a Zen 5 desktop part), the 20M-row
benchmark replica, result memo off, statements through the HTTP query
endpoint. Every comparison ran its binaries side by side: six rounds, each
round starting fresh processes and giving each binary a different copy of
the replica, ten interleaved runs of each of Q2-Q8 per round. A figure is
the median across rounds of the round's median, as a change against the
plain build in the same comparison; "all" is the geometric mean over Q2-Q8.
Small statements ran on one wire connection, 12,000 executions per case per
binary across three rounds of fresh processes.

Every binary gave the same answers to the 32 benchmark statements and
their cold variants (Q1-Q8, N1-N4), and the same bytes on the wire for a
recorded set of 300 small statements.

**The floor.** Three builds of one commit - two target directories, two
checkouts - have identical code, constants and unwind tables, byte for
byte; only the debug information differs (it records the target
directory). Measured against each other as if they were different builds,
they differ by -1.1% to +0.7% over all seven queries and by up to 8% on a
single query. That spread is between processes, not between builds: where
the address space and the allocator put things, and which copy of the data
a process reads. A single-query difference under about 8%, or an
all-queries difference under about 2%, between two builds is not a finding
on this harness.

## The release profile, one factor at a time

Build time is a cold `cargo build --release -p pintail`; size is the
binary with its line tables, and stripped.

| Build | Build time | Size (stripped) | All: median | All: CPU | Notes |
|---|---|---|---|---|---|
| as shipped (thin LTO, 1 unit, unwind, generic) | 159 s | 261 MB (53.7) | - | - | |
| `lto = "off"` | 124 s | 251 MB (50.3) | +1.7% | +1.4% | key lookup over the wire +13% |
| `lto = "fat"` | 273 s | 242 MB (49.4) | -1.1% | -0.4% | inside the floor, 72% longer build |
| `codegen-units = 16` | 145 s | 347 MB (65.7) | +4.8% | +6.8% | Q2 +23%, Q5 +8% |
| `panic = "abort"` | 153 s | 245 MB (50.0) | -0.7% | -0.9% | not an option, see below |
| control-plane crates at `opt-level = "s"` | 125 s | 252 MB (52.4) | +1.9% | +0.3% | key lookup +8% |
| `-C target-cpu=x86-64-v2` | 155 s | 260 MB (53.6) | -2.5% | -2.4% | Q2 -10% |
| `-C target-cpu=x86-64-v3` | 147 s | 260 MB (53.9) | -4.5% | -5.4% | Q2 -12%, Q5 -7%, Q4 -5% |
| `-C target-cpu=x86-64-v4` | 138 s | 261 MB (54.5) | -2.3% | -3.0% | Q2 -12%, the rest inside the floor |

The profile stays as it is. Nothing in `Cargo.toml` clears the floor on
most queries: fat LTO is the only setting that might be ahead and it is
inside the noise at 72% more build time; turning thin LTO off or raising
the codegen units loses; optimizing the control-plane crates for size
saves 34 seconds and costs a small statement 8%.

`panic = "abort"` cannot be used whatever it measures. The wire server
runs a bounded statement on the connection's own task inside
`catch_unwind`, so that a statement that panics answers its client with an
error and leaves every other connection alone. With `abort` the first such
panic ends the process.

The target level is the one build flag that moves a query beyond the
floor (Q2, and Q5 under v3), and v3 is ahead of v4 on this machine. It is
not a profile setting: a v3 binary does not start on a processor or a
hypervisor CPU model without AVX2.

## Profile-guided builds

`scripts/pgo-build.sh` instruments the server, trains it, and rebuilds
with the profile. The training workload is `benchmark/pgo-train.ts`. It
needs no source database: it starts the instrumented server on an empty
directory, creates a local database over the API, loads it over the MySQL
wire (2,000,000 orders in the benchmark's own distribution, 100,000 users,
10,000 products, 400,000 events), and runs the timed benchmark's
statements and cold variants, the wider shapes of the instruction suite,
statements under three session time zones, 20,000 small statements on one
connection with 5,000 prepared executions and 5,000 across four
connections, then restarts the server and repeats the analytical list
with the result memo on. Counts are checked against the generator's own
formula. It covers the write path (log, memtable, flush, compaction) and
startup recovery; it cannot cover snapshot copy or binlog decoding, which
need a source.

Cost on the measuring machine: instrumented build 136 s, training 214 s,
optimized build 144 s - a little over eight minutes, against 159 s for a
plain build. The binary grows from 53.7 MB to 57.5 MB stripped.

Compared with a profile trained the expensive way (the 20M-row replica's
statements and a replication burst from a live source), change against
the plain build:

| Query | 20M-trained | portable | portable, second comparison | portable + x86-64-v3 |
|---|---|---|---|---|
| Q2 | -7.3% | -5.5% | -5.3% | -18.7% |
| Q3 | -7.1% | -4.5% | -4.6% | -7.4% |
| Q4 | -2.1% | -0.3% | -2.3% | -12.7% |
| Q5 | -1.3% | -2.9% | -5.5% | -13.1% |
| Q6 | -2.3% | -1.9% | -2.3% | -6.1% |
| Q7 | -15.8% | -14.9% | -13.1% | -11.4% |
| Q8 | -3.7% | -1.5% | -4.8% | -6.4% |
| all, median | -5.8% | -4.6% | -5.5% | -10.9% |
| all, CPU | -4.4% | -4.7% | -4.4% | -9.8% |
| `SELECT 1+1`, one connection | -6.7% | -8.4% | -14.2% | -15.1% |
| key lookup, one connection | -8.6% | -6.7% | -6.7% | -12.0% |

The portable training keeps four fifths or more of what the 20M-row
training gives, and all of it on CPU time and on small statements, for a
run that fits in a container build. A profile-guided build for x86-64-v3
is the best binary measured: about 11% less time across the seven
queries, 19% on a filtered count, 12-15% on small statements. The two
effects add: v3 alone is -4.5%, the profile alone about -5%.

A later re-measurement on the current engine
(`benchmark/evidence/pgo-v3-measurement.md`: 18 interleaved rounds per
track, with a same-commit floor arm) put the portable profile-guided build
at -7.8% across Q1-Q8 (-8.8% CPU), -5.1% across eight time-window shapes,
and -10% to -16% on a key lookup over one connection, with every answer
identical. Adding x86-64-v3 took Q1-Q8 to -10.0%: 2.4 points more, for a
second binary, a launcher that has to read the processor's flags, and a
binary that dies with SIGILL if that choice is ever forced or wrong.

`PINTAIL_PGO_BOLT=1` adds a post-link layout pass. See the script's
header for what it needs; its measured state is in `docs/decisions.md`.

## Instruction counts

The 26-case instruction suite (`docs/instruction-counts.md`), built with
the same flags, against the plain build:

- **x86-64-v3** executes fewer instructions on the scan-and-fold shapes:
  -36% on the filtered count, -18% on the status and region groupings,
  -15% on the top-spenders grouping, -11% on the two-key grouping, -6% on
  the monthly aggregates. It executes 4-10% MORE on all eight time-window
  shapes (day and hour aggregates over a `DATETIME` and a zoned
  `TIMESTAMP`). Timed later on the server, none of them was slower on the
  clock under v3 (`benchmark/evidence/pgo-v3-measurement.md`).
- **The profile-guided build** executes 0-10% more instructions on most
  cases (17% more on the two-column text filter, 6% fewer on
  `COUNT(DISTINCT)`), with first-level cache misses unchanged, while
  taking less time on the clock: its gain is code placement and branch
  layout, which this suite does not model.
- **Fat LTO** is within 2% on every case but one (an hourly window, +7%).

Every case answered identically in every build.

## Telling builds apart

`scripts/ab-build.sh <out> <revision>...` builds revisions one after the
other in one directory with the paths mapped to fixed names. Two builds of
a revision are then the same file, byte for byte (`--check` proves it by
building the first revision twice), so a checksum answers "is this the
same build?". Building in a second directory still changes the checksum,
though not the code.

A running server names its own build: the startup line
`pintail optimizations:` carries `build_target=` (generic, x86-64-v2,
x86-64-v3, x86-64-v4) and `build_variant=` (standard, pgo, pgo+bolt).
The published image reports `build_target=generic build_variant=pgo`.

## What ships

One image per architecture (linux/amd64 and linux/arm64), each with one
binary at `/usr/local/bin/pintail`: profile-guided, compiled for that
platform's generic target, trained on `benchmark/pgo-train.ts`. It starts
on every machine the plain build started on. The vector kernels still pick
AVX2 or AVX-512 at run time where the processor has them (`PINTAIL_SIMD`
overrides that), so the generic binary keeps the part of a newer level
that matters most.

Why not the x86-64-v3 binary as well: over the portable profile-guided
build it measured 2.4 points across Q1-Q8 and about the same on small
statements (`benchmark/evidence/pgo-v3-measurement.md`). Shipping it meant a second
binary, a launcher choosing between them from `/proc/cpuinfo`, a gate run
for each binary, and a binary that dies with SIGILL before it logs
anything if the choice is ever wrong. One binary that runs everywhere is
worth more than those points.

How the image is built. The `Dockerfile`'s builder stage runs
`scripts/pgo-build.sh server` when `PINTAIL_PGO=1`, its default: an
instrumented build, the training run, a merge with `llvm-profdata`, and
the optimized build. Training happens inside the image build rather than
on a CI host before it, for two reasons: the binary is compiled, trained
and linked under the same toolchain and glibc as the runtime base (a
binary built on a newer host than the bookworm base asks for a newer
glibc than the base has), and the training needs no source server, so
nothing outside the build has to be arranged. The training tools and data
stay in the builder stage. `--build-arg PINTAIL_PGO=0` builds a plain
release binary; `docker-compose.dev.yml` builds plain unless
`PINTAIL_PGO=1` is set, and the compose gate builds the image as it ships
and fails unless the container reports `build_variant=pgo`.

The binary taken out of an image built this way was checked against the
one from a `PINTAIL_PGO=0` image on the 20M-row replica, on the same
machine, interleaved as above (6 rounds of 15 cycles, a third arm running
the plain binary again as the floor): -6.5% across Q1-Q8 (floor -1.8%),
-5.5% CPU over Q2-Q8 (floor -2.1%), Q7 -17.5%, and -11.4% on a key lookup
over one connection (floor -4.0%), with every answer equal to MySQL's. It
passed the end-to-end gate (7,061 checks, the same six documented-gap
warnings) and both upstream regression suites with the banked counts
(9,279 and 8,761 exact).

The release workflow passes `PINTAIL_PGO=1` explicitly and, on each
architecture's own runner, starts the image it just pushed and fails the
job unless the startup line says `build_target=generic build_variant=pgo`.

Cost, measured as cold image builds (empty build cache, base images
pulled) on an 8-vCPU Zen 5 machine:

| Image | Build | Image size |
|---|---|---|
| `PINTAIL_PGO=0` | 283 s | 472 MB |
| `PINTAIL_PGO=1` | 573 s: instrumented build 150 s, training 237 s, optimized build 158 s | 505 MB |

The profile-guided build skips the dependency pre-build (its flags differ,
so the cooked dependencies would go unused). On the release workflow's
four-vCPU hosted runners the two compilations take roughly twice as long,
so each architecture's job should grow by about ten minutes; the two jobs
run at the same time, so the release grows by the same. That is an
estimate from this machine, not a measured workflow run.

