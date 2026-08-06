# Pintail patch

This directory vendors `opensrv-mysql` 0.7.0 from upstream tag `v0.7.0`
(`66140cc266e1eb712a0821c112a3ec743f9cccd4`). The upstream release hardcodes
three client-visible `ColumnDefinition41` fields: column length to 1024,
character set to utf8, and decimal scale to zero.

Pintail's patch adds `column_length`, `character_set`, and `decimals` to
`Column` and writes those values into result metadata. No command parsing,
authentication, row encoding, or transport behavior is changed. The original
Apache-2.0 license is retained in `LICENSE`.
