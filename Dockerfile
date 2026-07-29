FROM oven/bun:1.3.14 AS dashboard

WORKDIR /source/packages/dashboard
COPY packages/dashboard/package.json packages/dashboard/bun.lock ./
RUN bun install --frozen-lockfile
COPY packages/dashboard/app ./app
COPY packages/dashboard/public ./public
COPY packages/dashboard/nuxt.config.ts ./
RUN bun run generate

FROM rust:1.94-bookworm AS builder

WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY --from=dashboard /source/packages/dashboard/.output/public \
    ./packages/dashboard/.output/public
ENV PINTAIL_DASHBOARD_PREBUILT=1
RUN cargo build --locked --release --package pintail

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 pintail \
    && install --directory --owner pintail --group pintail /var/lib/pintail

COPY --from=builder /source/target/release/pintail /usr/local/bin/pintail

USER pintail
VOLUME ["/var/lib/pintail"]
EXPOSE 8080
ENTRYPOINT ["pintail"]
CMD ["--data-dir", "/var/lib/pintail", "--http-bind", "0.0.0.0:8080"]
