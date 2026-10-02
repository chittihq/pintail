#!/bin/sh
# Starts the build of the server that fits this processor.
#
# An image built with PINTAIL_X86_64_V3=1 holds two binaries: the generic
# one, which runs on every x86-64 machine, and one compiled for x86-64-v3
# (AVX2, BMI2, FMA and the rest of that level). This script is the image's
# `pintail`: it reads the processor's flags, picks one and replaces itself
# with it, so the server is still the container's first process and gets
# its signals. `PINTAIL_BINARY=generic` or `=x86-64-v3` overrides the
# choice; the server's startup log names what is running (`build_target=`).
set -eu
directory=${PINTAIL_LIBEXEC:-/usr/local/lib/pintail}
choice=${PINTAIL_BINARY:-auto}
case "$choice" in
  auto)
    choice=generic
    if [ -x "$directory/pintail-x86-64-v3" ] && [ -r /proc/cpuinfo ]; then
      # The kernel lists a vector extension only when it also saves its
      # registers, so the flag alone is enough.
      flags=" $(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2) "
      choice=x86-64-v3
      for flag in avx avx2 bmi1 bmi2 f16c fma abm movbe xsave; do
        case "$flags" in
          *" $flag "*) ;;
          *) choice=generic ;;
        esac
      done
    fi
    ;;
  generic | x86-64-v3) ;;
  *)
    echo "PINTAIL_BINARY must be auto, generic or x86-64-v3, not $choice" >&2
    exit 2
    ;;
esac
exec "$directory/pintail-$choice" "$@"
