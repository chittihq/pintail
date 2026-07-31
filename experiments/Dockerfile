FROM rust:1.94-slim AS build
WORKDIR /lab
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /lab/target/release/e01-filter-repr /lab/target/release/e02-aggregation \
     /lab/target/release/e03-topk /lab/target/release/e04-join \
     /lab/target/release/e05-merge-on-read /lab/target/release/e06-decode \
     /lab/target/release/e07-strings /lab/target/release/e08-string-hash \
     /lab/target/release/e09-predicate-cache /lab/target/release/e10-parallel-scan \
     /lab/target/release/e11-sweep-line /lab/target/release/e12-normalized-keys \
     /usr/local/bin/
CMD ["/bin/sh", "-c", "nproc && for e in e01-filter-repr e02-aggregation e03-topk e04-join e05-merge-on-read e06-decode e07-strings e08-string-hash e09-predicate-cache e10-parallel-scan e11-sweep-line e12-normalized-keys; do $e; done"]
