FROM rust:1.97.1-bookworm
RUN apt-get update && apt-get install --yes --no-install-recommends valgrind python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/sqllogic ./tests/sqllogic
COPY benchmark/instructions ./benchmark/instructions
COPY scripts/pgo-build.sh ./scripts/pgo-build.sh
ARG SOURCE_COMMIT
ARG PINTAIL_PGO=0
# Only the commit identity is needed by the recorder; no remotes or owner config.
RUN test -n "$SOURCE_COMMIT" && printf '%s\n' "$SOURCE_COMMIT" > /source/source-commit
RUN if [ "$PINTAIL_PGO" = 1 ]; then \
      rustup component add llvm-tools-preview \
      && bash scripts/pgo-build.sh workload \
      && cp target/pgo/instruction_workload /usr/local/bin/instruction-workload; \
    elif [ "$PINTAIL_PGO" = 0 ]; then \
      cargo build --locked --release -p pintail-exec --example instruction_workload \
      && cp target/release/examples/instruction_workload /usr/local/bin/instruction-workload; \
    else echo 'PINTAIL_PGO must be 0 or 1' >&2; exit 2; fi
ENTRYPOINT ["python3", "benchmark/instructions/run.py", "--binary", "/usr/local/bin/instruction-workload"]
