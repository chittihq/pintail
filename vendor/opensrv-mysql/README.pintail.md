# Pintail patch

This directory vendors `opensrv-mysql` 0.7.0 from upstream tag `v0.7.0`
(`66140cc266e1eb712a0821c112a3ec743f9cccd4`). The upstream release hardcodes
three client-visible `ColumnDefinition41` fields: column length to 1024,
character set to utf8, and decimal scale to zero.

Pintail's patch adds `column_length`, `character_set`, and `decimals` to
`Column` and writes those values into result metadata. It also parses
`COM_RESET_CONNECTION`, `COM_STMT_RESET`, and `COM_CHANGE_USER`, exposes shim
hooks for session reset and reauthentication, supports an optional connection
idle timeout, observes peer disconnects while query/prepare/execute callbacks
are active, and turns malformed command packets into protocol errors rather
than dropping the connection silently. The original Apache-2.0 license is
retained in `LICENSE`.

For repository packaging, Pintail omits the upstream examples and development
dependencies, retains the upstream repository-level README in place of the
crate-specific README, and formats the vendored Rust sources with the local
toolchain. Those differences are mechanical and do not change runtime
behavior.
