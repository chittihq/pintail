# Read-semantics scope

The implementation target is agreement on read semantics. Fixture mutations,
stored-routine effects and MySQL-specific diagnostics are reported separately.
The full replay's selected files, statement identities, comparator, exact set,
refusals and original agreement denominator remain unchanged.

Generate a breakdown from a completed replay:

```sh
bun run tests/mtr/read-scope.ts validate-out/mtr/runs/<run>
```

`read-scope.json` records individual mismatch identities and the fixture reason
for a separate category. An unknown identity stays `read-or-unresolved`; its
filename alone never excludes it. A fixed identity disappears from the mismatch
breakdown automatically. The script reconciles every reported mismatch with the
original totals and refuses incomplete inventories.

This report does not calculate a read-only agreement percentage: that would
require classifying successful statements as well. Zero remaining read mismatches
would describe the compared reads only. Read statements that Pintail refuses, or
that cannot be compared after rejected setup, must still be reported; zero
mismatches is not a claim that every upstream read feature is implemented.
