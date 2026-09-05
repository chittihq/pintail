#!/bin/sh
# Single-pass release chain: every heavy stage runs exactly ONCE, its own
# artifacts are banked, and freshness + acceptance are then proven on the
# banked tree. Replaces the two-pass chain that ran e2e, bench and accept
# twice (once for evidence, once for the final validation) and cost an
# extra hour per release candidate.
#
# Ordering constraints this script encodes:
# - The heavy pass runs the `stable` PROFILE, which is the policy written
#   down in scripts/validate.ts. Its report says so, and only a run that
#   completes a whole profile is recorded as a complete gate.
# - freshness is ABSENT from the heavy pass and runs in the closing one: it
#   compares banked evidence against code commits, which can only pass
#   AFTER this run's artifacts are committed. It is the stable gate, and
#   this ordering is the only one in which it can be both honest and green.
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

bun run scripts/validate.ts --profile stable
bun run benchmark/run-tpch.ts
bun run benchmark/render-readme-table.ts

# Both v0.0.4 chain runs died on a fatal index.lock collision from a
# concurrent git process this script never identified (an IDE's background
# `git status` takes the lock briefly; a killed git leaves it stale).
# Retrying the write is race-free against any holder — waiting for a quiet
# moment is not — and a lock that never clears still fails loudly. Our own
# reads stop being writers too: with optional locks off, `git status`
# no longer writes back refreshed stat data.
GIT_OPTIONAL_LOCKS=0
export GIT_OPTIONAL_LOCKS

git_retry() {
  i=0
  until "$@"; do
    i=$((i + 1))
    if [ "$i" -ge 30 ]; then
      echo "git kept failing after $i attempts: $*"
      ls -l .git/index.lock 2>/dev/null || true
      return 1
    fi
    sleep 1
  done
}

bank() {
  message=$1
  shift
  git_retry git add "$@"
  git diff --cached --quiet || git_retry git commit -m "$message$label"
}
bank "test(e2e): bank the differential gate" tests/e2e/results.json tests/e2e/results.md
bank "test(e2e): bank the mysql80 leg" tests/e2e/results-mysql80.json tests/e2e/results-mysql80.md
bank "test(recovery): bank the recovery suite" tests/e2e/results-recovery.md
bank "perf(bench): bank the analytical benchmark and README table" benchmark/results.json benchmark/results.md benchmark/mysql-baseline.json README.md
bank "perf(bench): bank the TPC-H workload" benchmark/workloads/tpch-v1/results
bank "perf(bench): bank the production workload" benchmark/workloads/commerce-production-v1/results
if [ -n "$(git status --porcelain)" ]; then
  echo "unexpected changes left after banking:"
  git status --porcelain
  exit 1
fi

bun run scripts/validate.ts --stages=fmt,freshness,accept
# the confirming accept rewrites its ledgers once more; the banked copies
# from the same chain are the evidence of record
git_retry git checkout -- .
if [ -n "$(git status --porcelain)" ]; then
  echo "tree still dirty after the closing restore:"
  git status --porcelain
  exit 1
fi
ok=1
echo "RELEASE-CHAIN-DONE"
