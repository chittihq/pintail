FROM ubuntu:26.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3t64 && rm -rf /var/lib/apt/lists/*
COPY pintail /usr/local/bin/pintail
ENTRYPOINT ["/usr/local/bin/pintail"]
CMD ["--data-dir", "/data", "--http-bind", "0.0.0.0:8080", "--wire-bind", "0.0.0.0:3306"]
