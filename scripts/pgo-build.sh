#!/usr/bin/env bash
# Profile-guided build: instrument, train, merge, rebuild with the profile.
#
#   scripts/pgo-build.sh [server|workload]
#
# `server` (the default) builds the `pintail` binary and trains it with
# benchmark/pgo-train.ts: a deterministic workload that loads a local
# database over the wire and needs no source server, so it runs anywhere bun
# does, in a few minutes. `workload` builds only the four-statement executor
# example the instruction gate measures, trained on itself.
#
# The release image's builder stage runs `server` mode (Dockerfile,
# PINTAIL_PGO=1, the default there). Nothing here changes what a plain
# `cargo build --release` makes.
#
#   PINTAIL_TARGET_CPU=x86-64-v3   compile for that level instead of generic
#                                  (for measurement; the release ships generic)
#   PINTAIL_PGO_BOLT=1             also lay the binary out after linking
#                                  (needs llvm-bolt and merge-fdata on PATH)
#   PINTAIL_PGO_PROFILE=<file>     use this merged profile; skip training
#   PINTAIL_PGO_SAVE_PROFILE=<file>  keep the merged profile there
#   PINTAIL_PGO_ROWS / _EVENTS / _STATEMENTS   size of the training run
#
# The result is target/pgo/pintail (and target/pgo/instruction_workload in
# `workload` mode). The binary reports itself: its startup log says
# `build_variant=pgo` or `pgo+bolt`, and `build_target=` the level.
set -euo pipefail
pgo_mode=${1:-server}
case "$pgo_mode" in server|workload) ;; *) echo 'usage: pgo-build.sh [server|workload]' >&2; exit 2 ;; esac
pgo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
pgo_cargo=${CARGO:-${CARGO_HOME:-${HOME}/.cargo}/bin/cargo}
pgo_rustc=${RUSTC:-${CARGO_HOME:-${HOME}/.cargo}/bin/rustc}
pgo_sysroot=$("$pgo_rustc" --print sysroot)
pgo_host=$("$pgo_rustc" -vV | sed -n 's/^host: //p')
pgo_profdata="$pgo_sysroot/lib/rustlib/$pgo_host/bin/llvm-profdata"
if [[ ! -x "$pgo_profdata" ]]; then
  echo 'Install matching profiling tools: rustup component add llvm-tools-preview' >&2
  exit 2
fi
pgo_bolt=${PINTAIL_PGO_BOLT:-0}
pgo_bolt_tool=''
pgo_merge_tool=''
if [[ "$pgo_bolt" == 1 ]]; then
  [[ "$pgo_mode" == server ]] || { echo 'PINTAIL_PGO_BOLT applies to the server build' >&2; exit 2; }
  # Distributions install the tools versioned (llvm-bolt-20) or plain.
  for pgo_name in llvm-bolt $(compgen -c | grep -E '^llvm-bolt-[0-9]+$' | sort -Vr); do
    if command -v "$pgo_name" >/dev/null; then pgo_bolt_tool=$pgo_name; break; fi
  done
  for pgo_name in merge-fdata $(compgen -c | grep -E '^merge-fdata-[0-9]+$' | sort -Vr); do
    if command -v "$pgo_name" >/dev/null; then pgo_merge_tool=$pgo_name; break; fi
  done
  if [[ -z "$pgo_bolt_tool" || -z "$pgo_merge_tool" ]]; then
    echo 'PINTAIL_PGO_BOLT=1 needs llvm-bolt and merge-fdata on PATH' >&2
    exit 2
  fi
fi
if [[ "$pgo_mode" == server && -z "${PINTAIL_PGO_PROFILE:-}" ]] && ! command -v bun >/dev/null; then
  echo 'The training workload runs under bun; install it or pass PINTAIL_PGO_PROFILE' >&2
  exit 2
fi

cd "$pgo_root"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
mkdir -p "$CARGO_TARGET_DIR"
pgo_target=$(cd "$CARGO_TARGET_DIR" && pwd)
pgo_data=$(mktemp -d "$pgo_target/pgo-profile.XXXXXX")
trap 'rm -rf "$pgo_data"' EXIT
pgo_out="$pgo_target/$pgo_host/release"
pgo_flags=${RUSTFLAGS:-}
if [[ -n "${PINTAIL_TARGET_CPU:-}" ]]; then pgo_flags="$pgo_flags -Ctarget-cpu=$PINTAIL_TARGET_CPU"; fi
if [[ "$pgo_mode" == server ]]; then
  pgo_args=(build --locked --release --target "$pgo_host" -p pintail --bin pintail)
else
  pgo_args=(build --locked --release --target "$pgo_host" -p pintail-exec --example instruction_workload)
fi
pgo_seconds() { printf 'PGO-TIME %s %s s\n' "$1" "$(( $(date +%s) - $2 ))"; }

# Runs the training workload against one binary. The caller says where the
# binary writes what it recorded.
pgo_train() {
  if [[ "$pgo_mode" == workload ]]; then
    for _ in 1 2 3; do RAYON_NUM_THREADS=1 "$1" all >/dev/null; done
    return
  fi
  if [[ ! -d benchmark/node_modules ]]; then (cd benchmark && bun install --frozen-lockfile); fi
  bun run benchmark/pgo-train.ts "$1"
}

pgo_merged=${PINTAIL_PGO_PROFILE:-}
if [[ -z "$pgo_merged" ]]; then
  pgo_started=$(date +%s)
  RUSTFLAGS="$pgo_flags -Cprofile-generate=$pgo_data" "$pgo_cargo" "${pgo_args[@]}"
  pgo_seconds instrumented-build "$pgo_started"
  if [[ "$pgo_mode" == server ]]; then pgo_binary="$pgo_out/pintail"; else pgo_binary="$pgo_out/examples/instruction_workload"; fi
  pgo_started=$(date +%s)
  LLVM_PROFILE_FILE="$pgo_data/%m-%p.profraw" pgo_train "$pgo_binary"
  pgo_seconds training "$pgo_started"
  pgo_merged="$pgo_data/merged.profdata"
  "$pgo_profdata" merge --sparse "$pgo_data"/*.profraw -o "$pgo_merged"
  if [[ -n "${PINTAIL_PGO_SAVE_PROFILE:-}" ]]; then cp "$pgo_merged" "$PINTAIL_PGO_SAVE_PROFILE"; fi
fi

pgo_variant=pgo
pgo_use="$pgo_flags -Cprofile-use=$pgo_merged"
if [[ "$pgo_bolt" == 1 ]]; then
  pgo_variant=pgo+bolt
  # The layout pass moves functions, which needs the relocations kept.
  pgo_use="$pgo_use -Clink-arg=-Wl,--emit-relocs"
fi
pgo_started=$(date +%s)
PINTAIL_BUILD_VARIANT=$pgo_variant RUSTFLAGS="$pgo_use" "$pgo_cargo" "${pgo_args[@]}"
pgo_seconds optimized-build "$pgo_started"
mkdir -p "$pgo_target/pgo"

if [[ "$pgo_mode" == workload ]]; then
  "$pgo_out/examples/instruction_workload" all
  cp "$pgo_out/examples/instruction_workload" "$pgo_target/pgo/instruction_workload"
  printf 'PGO-BUILD-DONE: %s\n' "$pgo_mode"
  exit 0
fi

if [[ "$pgo_bolt" == 1 ]]; then
  pgo_started=$(date +%s)
  mkdir -p "$pgo_data/bolt"
  "$pgo_bolt_tool" "$pgo_out/pintail" -o "$pgo_data/pintail-bolt-instrumented" \
    -instrument -instrumentation-file-append-pid -instrumentation-file="$pgo_data/bolt/prof" >/dev/null
  PINTAIL_PGO_STATEMENTS=${PINTAIL_PGO_STATEMENTS:-8000} pgo_train "$pgo_data/pintail-bolt-instrumented"
  "$pgo_merge_tool" "$pgo_data"/bolt/prof.*.fdata > "$pgo_data/bolt/merged.fdata" 2>/dev/null
  "$pgo_bolt_tool" "$pgo_out/pintail" -o "$pgo_target/pgo/pintail" -data="$pgo_data/bolt/merged.fdata" \
    -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions -split-all-cold -split-eh \
    -icf=1 -use-gnu-stack -dyno-stats > "$pgo_target/pgo/bolt.log"
  pgo_seconds layout "$pgo_started"
else
  cp "$pgo_out/pintail" "$pgo_target/pgo/pintail"
fi
printf 'PGO-BUILD-DONE: %s %s\n' "$pgo_mode" "$pgo_variant"
