#!/bin/sh
# Single-pass release chain: every heavy stage runs exactly ONCE, its own
# artifacts are banked, and freshness + acceptance are then proven on the
# banked tree. Replaces the two-pass chain that ran e2e, bench and accept
# twice (once for evidence, once for the final validation) and cost an
# extra hour per release candidate.
#
# Ordering constraints this script encodes:
# - fmt is ABSENT from the heavy pass: its evidence-freshness gate compares
#   banked evidence against code commits, which can only pass AFTER this
#   run's artifacts are committed.
# - TPC-H runs before banking so its results join the same bank set; the
#   validate harness has no tpch stage of its own.
# - accept re-runs at the end on the clean banked tree - the release
#   checklist's final proof - and is cheap the second time (warm image).
#
# Requires DOCKER_HOST pointing at the shared Docker host. Optional
# RELEASE_LABEL suffixes the bank commit subjects.
set -e
ok=""
trap '[ "$ok" = 1 ] || echo "RELEASE-CHAIN-FAIL"' EXIT
cd "$(dirname "$0")/.."
label="${RELEASE_LABEL:+ for $RELEASE_LABEL}"

bun run scripts/validate.ts --stages=unit,oracle,e2e,e2e-mysql80,browser,bench,accept
bun run benchmark/run-tpch.ts
bun run benchmark/render-readme-table.ts

bank() {
  git add $2
  git diff --cached --quiet || git commit -m "$1$label"
}
bank "test(e2e): bank the differential gate" "tests/e2e/results.json tests/e2e/results.md"
bank "test(e2e): bank the mysql80 leg" "tests/e2e/results-mysql80.json tests/e2e/results-mysql80.md"
bank "perf(bench): bank the analytical benchmark and README table" "benchmark/results.json benchmark/results.md benchmark/mysql-baseline.json README.md"
bank "perf(bench): bank the TPC-H workload" "benchmark/workloads/tpch-v1/results"
bank "perf(bench): bank the production workload" "benchmark/workloads/commerce-production-v1/results"
if [ -n "$(git status --porcelain)" ]; then
  echo "unexpected changes left after banking:"
  git status --porcelain
  exit 1
fi

bun run scripts/validate.ts --stages=fmt,accept
# the confirming accept rewrites its ledgers once more; the banked copies
# from the same chain are the evidence of record
git checkout -- .
ok=1
echo "RELEASE-CHAIN-DONE"
