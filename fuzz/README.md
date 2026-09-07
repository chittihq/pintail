# Parser fuzzing

These targets use the same wire/storage code and MySQL decoder version as
Pintail. They need no server, credentials, network connection or Docker.
The separate lockfile is tracked. Keep its MySQL decoder version aligned with
`Cargo.lock`; the pinned mysql_async 0.37.1 uses the same decoder as the workspace.

Local deterministic smoke checks (stable Rust):

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test --manifest-path fuzz/Cargo.toml --locked
```

Coverage-guided fuzzing uses [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz/tutorial.html):

```sh
cargo install cargo-fuzz --locked --version 0.13.2
rustup toolchain install nightly --profile minimal
CARGO_TARGET_DIR=target cargo +nightly fuzz run wire --features libfuzzer -- -max_total_time=60 -max_len=65536
CARGO_TARGET_DIR=target cargo +nightly fuzz run storage --features libfuzzer -- -max_total_time=60 -max_len=65536
CARGO_TARGET_DIR=target cargo +nightly fuzz run binlog --features libfuzzer -- -max_total_time=60 -max_len=65536
```

- `wire`: packet framing, pre-login handshake, commands and parameter type
  decoding. Finite input returns EOF rather than waiting on a socket.
- `storage`: row and WAL-batch decoding without their checksum envelopes, plus
  the complete read-only WAL file reader. Only a target-owned temporary file is
  created. Checksums cannot prevent mutations reaching the record decoders.
- `binlog`: binlog-v4 event bodies with a coherent bounded event-length header.
  It does not fuzz the transport assembler or compressed payload expansion.
  It uses the dependency's event decoder, including event types Pintail ignores.

The seeds include a handshake, query event and encoded stored row. Keep useful
minimized regressions in the corpus; transient libFuzzer artifacts and newly
expanded corpus entries stay ignored. A smoke run is bounded evidence, not a
claim of exhaustive parser safety. No target catches panics.

## Transaction-payload regression

The decoder is pinned through a local patch shared by the workspace and this
harness. Transaction-payload header field IDs above 255 are rejected before
the narrowing conversion, including when the stream decodes a payload before
returning its event to CDC. The minimized artifact is checked in under
`corpus/binlog/transaction_payload_field`; its deterministic regression runs
in ordinary smoke checks and asserts an invalid-data error:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test --manifest-path fuzz/Cargo.toml transaction_payload_field
```

The unrestricted binlog fuzz target is unchanged. Fixing this reproducer is
not a claim that all malformed inputs are safe.
