FROM oven/bun:1.3.14 AS dashboard

WORKDIR /source/packages/dashboard
COPY packages/dashboard/package.json packages/dashboard/bun.lock ./
RUN bun install --frozen-lockfile
COPY packages/dashboard/app ./app
COPY packages/dashboard/public ./public
COPY packages/dashboard/nuxt.config.ts ./
ARG SENTRY_ORG
ARG SENTRY_PROJECT
RUN --mount=type=secret,id=sentry_auth_token,env=SENTRY_AUTH_TOKEN bun run generate

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
# The release binary is profile-guided: built instrumented, trained by
# benchmark/pgo-train.ts (it loads a local database over the wire, so no
# source server is needed), then rebuilt with the profile, for the generic
# target of the platform being built. The training runs inside this stage so
# the binary is compiled and trained against the same toolchain and glibc as
# the runtime base below. It adds an instrumented build and a training run to
# the image build; `--build-arg PINTAIL_PGO=0` builds a plain release binary
# instead, for local images that do not need the last few percent.
ARG PINTAIL_PGO=1
COPY --from=planner /source/recipe.json recipe.json
# Rebuilds only when Cargo.lock changes. A profile-guided build compiles its
# dependencies with its own flags into its own target directory, so the
# cooked dependencies would go unused: it skips this step.
RUN --mount=type=cache,target=/source/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    if [ "$PINTAIL_PGO" = 0 ]; then \
      cargo chef cook --locked --release --package pintail --recipe-path recipe.json; \
    fi
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY tests/sqllogic ./tests/sqllogic
COPY scripts/pgo-build.sh ./scripts/
# The training workload and the runtime it runs under. Neither reaches the
# runtime image.
COPY benchmark/package.json benchmark/bun.lock benchmark/pgo-train.ts benchmark/queries.ts ./benchmark/
COPY --from=dashboard /usr/local/bin/bun /usr/local/bin/bun
COPY --from=dashboard /source/packages/dashboard/.output/public \
    ./packages/dashboard/.output/public
ENV PINTAIL_DASHBOARD_PREBUILT=1
# The chef layer caches the 472 dependencies, but the workspace's own 17
# crates recompiled from scratch on every source change. A BuildKit cache
# mount keeps the incremental state between builds; the binary is copied out
# inside the same RUN because cache mounts do not persist into the layer.
# The cache mount outlives the tree it was built from, and a COPY layer
# carries the source files' own timestamps. When a tree older by mtime is
# built after a newer one, cargo reads the newer artifacts as fresh and
# links a stale workspace crate. Stamping the sources at build time makes
# every workspace crate newer than whatever the mount holds; the 472
# dependencies keep their cache.
RUN --mount=type=cache,target=/source/target,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    find crates tests/sqllogic -name '*.rs' -exec touch {} + \
    && mkdir -p /out/bin \
    && if [ "$PINTAIL_PGO" = 1 ]; then \
      rustup component add llvm-tools-preview \
      && bash scripts/pgo-build.sh server \
      && cp /source/target/pgo/pintail /out/bin/pintail; \
    elif [ "$PINTAIL_PGO" = 0 ]; then \
      cargo build --locked --release --package pintail \
      && cp /source/target/release/pintail /out/bin/pintail; \
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

# The one binary, at /usr/local/bin/pintail. Its startup line
# `pintail optimizations:` says `build_variant=pgo` when it is profile-guided.
COPY --from=builder /out/ /usr/local/

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
