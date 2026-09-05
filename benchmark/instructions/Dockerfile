FROM rust:1.97.1-bookworm
RUN apt-get update && apt-get install --yes --no-install-recommends valgrind python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/sqllogic ./tests/sqllogic
COPY benchmark/instructions ./benchmark/instructions
ARG SOURCE_COMMIT
# Only the commit identity is needed by the recorder; no remotes or owner config.
RUN test -n "$SOURCE_COMMIT" && printf '%s\n' "$SOURCE_COMMIT" > /source/source-commit
RUN cargo build --locked --release -p pintail-exec --example instruction_workload
ENTRYPOINT ["python3", "benchmark/instructions/run.py", "--binary", "target/release/examples/instruction_workload"]
