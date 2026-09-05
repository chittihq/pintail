#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../../.."
export CARGO_TARGET_DIR=target
recovery_cargo="${CARGO:-${HOME}/.cargo/bin/cargo}"
# The normal unit gate covers default-off builds. These checks additionally
# exercise the feature-enabled facade and subprocess storage/metadata faults.
"$recovery_cargo" clippy --workspace --all-targets --all-features -- -D warnings
"$recovery_cargo" test -p pintail-failpoint -p pintail-store -p pintail-meta --all-features
cd tests/e2e
bunx --no-install tsc --project recovery/tsconfig.json
bun test recovery/policy.test.ts recovery/proxy.test.ts recovery/harness.test.ts
bun run recovery/run.ts "$@"
