FROM rust:1.94-slim AS build
WORKDIR /lab
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /lab/target/release/e01-filter-repr /usr/local/bin/
COPY --from=build /lab/target/release/e02-aggregation /usr/local/bin/
COPY --from=build /lab/target/release/e03-topk /usr/local/bin/
COPY --from=build /lab/target/release/e04-join /usr/local/bin/
COPY --from=build /lab/target/release/e05-merge-on-read /usr/local/bin/
CMD ["/bin/sh", "-c", "nproc && e01-filter-repr && e02-aggregation && e03-topk && e04-join && e05-merge-on-read"]
