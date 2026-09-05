# Executor instruction gate

Four fixed 4,096-row queries cover filtering, grouped aggregation, a join and
TopK. An independent Rust calculation checks each full answer. Callgrind counts
only the non-inlined `instruction_workload::measured` function and its callees,
including parse/bind/plan/execute, excluding dataset setup and oracle calculation.
Three samples per query produce a median; each query must stay within 5% of its
baseline. Improvements elsewhere cannot conceal one query's regression.

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --locked --release -p pintail-exec --example instruction_workload
python3 benchmark/instructions/run.py --binary target/release/examples/instruction_workload --output validate-out/instructions.json
```

Linux, Python 3 and Valgrind are required. On another OS, build the provided
Dockerfile with `docker build -f benchmark/instructions/Dockerfile --build-arg
SOURCE_COMMIT=$(git rev-parse HEAD) -t pintail-instructions .`, then run
`docker run --rm pintail-instructions --record --output /tmp/counts.json`.
The JSON is printed to stdout for capture even with a remote daemon; no bind
mount or privileged performance-counter access is required.

The banked baseline uses rustc 1.97.1 and Valgrind 3.19.0 on Linux x86_64.
The measurement CI job pins that compiler independently of the normal test
toolchain so a repository toolchain file cannot silently change the comparison.

Use `--record --label <build>` to collect a candidate baseline without claiming
a pass. Review and explicitly bank `baseline-linux.json` after measurement.
Architecture, compiler, Valgrind, workload hash and thread count must match for
a comparison. Toolchain changes require new evidence, not a relaxed threshold.
The baseline and every result also identify the source commit and binary hash.

This gate is a regression signal for these queries, not a prediction of latency
or cache/IO behavior. Hash-table iteration can introduce small instruction
variation; the explicit threshold allows that. Keep the full timed benchmark
for published speed claims. See the [Callgrind manual](https://valgrind.org/docs/manual/cl-manual.html)
for collection-boundary behavior.

## Release settings and PGO

`release-linux.json` measures the explicit Thin-LTO/single-codegen-unit profile:
5.91% fewer total instructions than the original release settings across these
four queries, with no per-query regression. This is instruction evidence for
this workload, not a claim about overall server latency.

Build the instruction image with `--build-arg PINTAIL_PGO=1` to train and measure
PGO. Compare its result against `release-linux.json` to isolate PGO from the
release-profile change. The production Dockerfile accepts the same opt-in
argument and uses the same rustc 1.97.1 compiler. Default images use the explicit
release profile without PGO.

For a local build, install `rustup component add llvm-tools-preview`, then run
`CARGO_TARGET_DIR=target bash scripts/pgo-build.sh server` (or `workload` for only
the measured executable). Outputs go under `target/pgo/`. Training runs all four
answer-checked queries three times. Profile generation and use employ the same
compiler, target and flags; an explicit target keeps host build scripts out of
the profile. Only temporary profiling data is removed. See the
[rustc PGO guide](https://doc.rust-lang.org/rustc/profile-guided-optimization.html).

The workload exercises the shared executor, not authentication, snapshot, CDC,
polling or every SQL family. That coverage limit is why PGO remains opt-in even
when these instruction comparisons improve.
