#!/usr/bin/env bash
# The per-slice check: rustfmt, workspace clippy as the gate runs it, and
# the tests of the crates a change touched, built as the gate builds them.
#
#   scripts/verify-crates.sh pintail-cdc pintail-sql pintail-exec
#
# With no crate named, every crate's tests run. Each step's output goes to
# verify-out/<step>.log; the lines printed here are the verdict. This is
# not a gate: `bun run scripts/validate.ts --profile ...` is.
set -u
cd "$(dirname "$0")/.."
out=verify-out
mkdir -p "$out"
packages=()
for crate in "$@"; do packages+=(-p "$crate"); done
[ ${#packages[@]} -eq 0 ] && packages=(--workspace)
failed=0
step() { # <name> <command...>
  local name=$1
  shift
  if "$@" > "$out/$name.log" 2>&1; then
    echo "$name: ok"
  else
    echo "$name: FAILED (see $out/$name.log)"
    failed=1
  fi
}
git log --oneline -1
step fmt cargo fmt --all --check
step clippy cargo clippy --workspace --all-targets -- -D warnings
step tests cargo nextest run --cargo-profile recovery "${packages[@]}"
grep -E '^\s+Summary' "$out/tests.log" | tail -1
[ $failed -eq 0 ] && echo "VERIFY-PASS" || echo "VERIFY-FAIL"
exit $failed
