# Parser fuzzing

These targets use the same wire/storage code and MySQL decoder version as
Pintail. They need no server, credentials, network connection or Docker.
The separate lockfile is tracked. Keep its MySQL decoder version aligned with
`Cargo.lock`; the pinned mysql_async 0.37.1 uses the same decoder as the workspace.
After changing path dependencies or dependency patches, refresh this lockfile too.
All validation profiles run the locked deterministic corpus as `parser-corpus`.

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

Both `TryFrom<u64>` impls for the transaction-payload header narrowed an
unrecognised value to `u8` with `unwrap()` before putting it in the error
that reports it, so a value above 255 panicked on exactly the input the
error exists for. `mysql_async` decodes those events inside its own binlog
stream, before an event reaches Pintail, so nothing on our side could
prevent it - the fix has to be in the decoder. The workspace and this
harness pin a fork carrying it (`Cargo.toml`, `[patch.crates-io]`);
upstream 0.38 still has both unwraps.

Pintail also refuses such a header in
`pintail_cdc::check_transaction_payload_header` before handing the event
on, which adds the event's position to the report and covers anything that
reaches `decode_event` directly. It is the second line, not the fix.

The minimized artifact is checked in under
`corpus/binlog/transaction_payload_field`; its field id is 256. The
deterministic regression asserts that the stream's own decode returns an
invalid-data error rather than aborting:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test --manifest-path fuzz/Cargo.toml transaction_payload
```

The unrestricted binlog fuzz target is unchanged. Fixing this reproducer is
not a claim that all malformed inputs are safe.
