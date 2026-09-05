#!/usr/bin/env bash
# Train the shared executor, then rebuild with the same compiler and flags.
set -euo pipefail
pgo_mode=${1:-server}
case "$pgo_mode" in server|workload) ;; *) echo 'usage: pgo-build.sh [server|workload]' >&2; exit 2 ;; esac
pgo_cargo=${CARGO:-${CARGO_HOME:-${HOME}/.cargo}/bin/cargo}
pgo_rustc=${RUSTC:-${CARGO_HOME:-${HOME}/.cargo}/bin/rustc}
pgo_sysroot=$("$pgo_rustc" --print sysroot)
pgo_host=$("$pgo_rustc" -vV | sed -n 's/^host: //p')
pgo_profdata="$pgo_sysroot/lib/rustlib/$pgo_host/bin/llvm-profdata"
if [[ ! -x "$pgo_profdata" ]]; then
  echo 'Install matching profiling tools: rustup component add llvm-tools-preview' >&2
  exit 2
fi
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
mkdir -p "$CARGO_TARGET_DIR"
pgo_target=$(cd "$CARGO_TARGET_DIR" && pwd)
pgo_data=$(mktemp -d "$pgo_target/pgo-profile.XXXXXX")
trap 'rm -rf "$pgo_data"' EXIT
pgo_flags=${RUSTFLAGS:-}
pgo_args=(build --locked --release --target "$pgo_host" -p pintail-exec --example instruction_workload)
if [[ "$pgo_mode" == server ]]; then pgo_args+=(-p pintail --bin pintail); fi
RUSTFLAGS="$pgo_flags -Cprofile-generate=$pgo_data" "$pgo_cargo" "${pgo_args[@]}"
for pgo_iteration in 1 2 3; do
  LLVM_PROFILE_FILE="$pgo_data/%m-%p.profraw" RAYON_NUM_THREADS=1 \
    "$pgo_target/$pgo_host/release/examples/instruction_workload" all
done
"$pgo_profdata" merge --sparse "$pgo_data"/*.profraw -o "$pgo_data/merged.profdata"
RUSTFLAGS="$pgo_flags -Cprofile-use=$pgo_data/merged.profdata" "$pgo_cargo" "${pgo_args[@]}"
"$pgo_target/$pgo_host/release/examples/instruction_workload" all
mkdir -p "$pgo_target/pgo"
cp "$pgo_target/$pgo_host/release/examples/instruction_workload" "$pgo_target/pgo/instruction_workload"
if [[ "$pgo_mode" == server ]]; then cp "$pgo_target/$pgo_host/release/pintail" "$pgo_target/pgo/pintail"; fi
printf 'PGO-BUILD-DONE: %s\n' "$pgo_mode"
