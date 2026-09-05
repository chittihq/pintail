# Parser fuzzing

These targets use the same wire/storage code and MySQL decoder version as
Pintail. They need no server, credentials, network connection or Docker.
The separate lockfile is tracked. Keep its MySQL decoder version aligned with
`Cargo.lock`; the pinned mysql_async 0.37.0 remains resolvable from that lockfile.

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

## Known dependency finding

The binlog target exposes a panic in mysql_common 0.37.3: a transaction-payload
header field ID above 255 reaches a narrowing conversion with `unwrap()`.
The same conversion was still present in upstream master when inspected on
2026-09-05. This is a source/binlog decoder finding, not the pre-login wire
parser. Dependency vendoring is outside this slice.

The deterministic regression is explicitly ignored in ordinary smoke checks
because it currently **fails**. Reproduce it without fuzz tooling:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test --manifest-path fuzz/Cargo.toml transaction_payload_field -- --ignored
```

The unrestricted binlog fuzz target remains enabled and reports crashes; it
is not claimed green. Remove the ignore only after updating or fixing the
production dependency and proving the reproducer no longer panics.
