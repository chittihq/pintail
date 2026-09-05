FROM oven/bun:1.3.14 AS dashboard

WORKDIR /source/packages/dashboard
COPY packages/dashboard/package.json packages/dashboard/bun.lock ./
RUN bun install --frozen-lockfile
COPY packages/dashboard/app ./app
COPY packages/dashboard/public ./public
COPY packages/dashboard/nuxt.config.ts ./
RUN bun run generate

# Dependencies compile in their own layer, keyed on the manifests alone.
# Copying sources before building — the obvious shape — puts 472 crates behind
# a layer that any source edit invalidates, so every image rebuilt the whole
# dependency graph from scratch. cargo-chef is a build-time tool only; nothing
# it produces is linked into the binary.
FROM rust:1.97.1-bookworm AS chef
RUN cargo install cargo-chef --locked --version ^0.1
WORKDIR /source

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/sqllogic ./tests/sqllogic
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG PINTAIL_PGO=0
COPY --from=planner /source/recipe.json recipe.json
# Rebuilds only when Cargo.lock changes.
RUN --mount=type=cache,target=/source/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    cargo chef cook --locked --release --package pintail --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/sqllogic ./tests/sqllogic
COPY scripts/pgo-build.sh ./scripts/pgo-build.sh
COPY --from=dashboard /source/packages/dashboard/.output/public \
    ./packages/dashboard/.output/public
ENV PINTAIL_DASHBOARD_PREBUILT=1
# The chef layer caches the 472 dependencies, but the workspace's own 17
# crates recompiled from scratch on every source change. A BuildKit cache
# mount keeps the incremental state between builds; the binary is copied out
# inside the same RUN because cache mounts do not persist into the layer.
RUN --mount=type=cache,target=/source/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    if [ "$PINTAIL_PGO" = 1 ]; then \
      rustup component add llvm-tools-preview \
      && bash scripts/pgo-build.sh server \
      && cp /source/target/pgo/pintail /usr/local/bin/pintail-built; \
    elif [ "$PINTAIL_PGO" = 0 ]; then \
      cargo build --locked --release --package pintail \
      && cp /source/target/release/pintail /usr/local/bin/pintail-built; \
    else echo 'PINTAIL_PGO must be 0 or 1' >&2; exit 2; fi

FROM debian:bookworm-slim

# The spill directory is created here as well as the data directory, even
# though spill lives inside the data directory by default. Docker only copies
# ownership into a fresh named volume when the mount point already exists in
# the image; mounted against a missing path it creates the directory as root,
# and this container does not run as root, so the server cannot write to it.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 pintail \
    && install --directory --owner pintail --group pintail /var/lib/pintail \
    && install --directory --owner pintail --group pintail /var/lib/pintail/spill

COPY --from=builder /usr/local/bin/pintail-built /usr/local/bin/pintail

# jemalloc (the binary's allocator) returns freed pages after a second
# instead of its ten-second default, from a background thread so the purge
# never rides on a query's allocation. The _RJEM_ prefix is the symbol
# prefix the Rust binding builds jemalloc with.
ENV _RJEM_MALLOC_CONF=background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0

USER pintail
VOLUME ["/var/lib/pintail"]
EXPOSE 8080 3306
ENTRYPOINT ["pintail"]
CMD ["--data-dir", "/var/lib/pintail", "--http-bind", "0.0.0.0:8080", "--wire-bind", "0.0.0.0:3306"]
