# Local decoder patch

`mysql_common` is pinned at 0.37.3 with one change in
`src/binlog/events/transaction_payload_event.rs`: reject a header field ID
above 255 before its narrowing conversion. Both Cargo workspaces use this
copy. Package metadata and license files are retained.

Keep the deterministic regression in `fuzz` and the CDC event tests when
removing the patch after a fixed dependency release. The unrestricted fuzz
target must continue exercising the complete decoder.
