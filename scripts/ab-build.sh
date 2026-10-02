#!/usr/bin/env bash
# Release binaries of several revisions, built so that only the code differs.
#
#   scripts/ab-build.sh [--check] <out-dir> <revision> [<revision> ...]
#
# Two builds of one commit already carry the same code: with one codegen
# unit, the instructions, constants and unwind tables come out byte for
# byte the same from any checkout and any target directory. What differs is
# the debug information, which records the target directory and the cargo
# home, so the files' checksums differ and "is this the same binary?" has
# no cheap answer.
#
# Here every revision is exported to one directory and built into one
# target directory, one after the other, with those paths mapped to fixed
# names and the build date pinned to the commit's. Building a revision
# twice then gives one file, byte for byte, and a checksum says whether two
# binaries are the same build. `--check` proves it on this machine by
# building the first revision a second time from an empty target directory.
#
# That matters for measuring: two PROCESSES of one binary differ by a few
# percent on a query that takes tens of milliseconds (address-space layout,
# where the allocator put things, which copy of the data they read). A
# difference of that size between two builds of one commit is the
# measurement, not the build - and with identical checksums that is proven
# rather than assumed. Compare builds across several fresh processes each.
#
# Results: <out-dir>/pintail-<short hash>, and <out-dir>/SHA256SUMS.
# Extra RUSTFLAGS and cargo profile variables in the environment apply to
# every revision alike. Run it on a build machine, not on a laptop.
set -euo pipefail
ab_check=0
if [[ "${1:-}" == --check ]]; then ab_check=1; shift; fi
if [[ $# -lt 2 ]]; then
  echo 'usage: ab-build.sh [--check] <out-dir> <revision> [<revision> ...]' >&2
  exit 2
fi
ab_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$1"
ab_out=$(cd "$1" && pwd)
shift
ab_cargo=${CARGO:-${CARGO_HOME:-${HOME}/.cargo}/bin/cargo}
ab_home=${CARGO_HOME:-${HOME}/.cargo}
ab_source="$ab_out/build/source"
ab_target="$ab_out/build/target"
ab_flags="${RUSTFLAGS:-} --remap-path-prefix=$ab_source=/pintail --remap-path-prefix=$ab_target=/target --remap-path-prefix=$ab_home=/cargo"

ab_build() {
  local revision=$1 name=$2
  rm -rf "$ab_source"
  mkdir -p "$ab_source"
  git -C "$ab_root" archive "$revision" | tar -x -C "$ab_source"
  # The dashboard is embedded in the binary; every revision gets this
  # checkout's build of it, so it is not a difference between them.
  if [[ -d "$ab_root/packages/dashboard/.output" ]]; then
    mkdir -p "$ab_source/packages/dashboard"
    cp -R "$ab_root/packages/dashboard/.output" "$ab_source/packages/dashboard/"
  fi
  (
    cd "$ab_source"
    SOURCE_DATE_EPOCH=$(git -C "$ab_root" show -s --format=%ct "$revision") \
      CARGO_INCREMENTAL=0 PINTAIL_DASHBOARD_PREBUILT=1 CARGO_TARGET_DIR="$ab_target" RUSTFLAGS="$ab_flags" \
      "$ab_cargo" build --locked --release -p pintail
  )
  cp "$ab_target/release/pintail" "$ab_out/$name"
}

: > "$ab_out/SHA256SUMS"
ab_first=''
for ab_revision in "$@"; do
  ab_hash=$(git -C "$ab_root" rev-parse --short=8 "$ab_revision^{commit}")
  ab_build "$ab_hash" "pintail-$ab_hash"
  ab_first=${ab_first:-$ab_hash}
done
if [[ "$ab_check" == 1 ]]; then
  rm -rf "$ab_target"
  ab_build "$ab_first" "pintail-$ab_first.again"
  if ! cmp -s "$ab_out/pintail-$ab_first" "$ab_out/pintail-$ab_first.again"; then
    echo "AB-BUILD-FAIL: two builds of $ab_first differ on this machine" >&2
    exit 1
  fi
  rm "$ab_out/pintail-$ab_first.again"
  echo "two builds of $ab_first are identical"
fi
(cd "$ab_out" && sha256sum pintail-* > SHA256SUMS)
cat "$ab_out/SHA256SUMS"
echo AB-BUILD-DONE
