# Validation report — 2026-09-10T07:09:36.763Z

Verdict: **PASS**
Profile: **rc** — complete
Claim when complete: rc correctness gates passed, on both MySQL majors the release claims to cover
HEAD: b2cb6f5b fix(exec): preflight every composite probe key allocation — 1 uncommitted path(s) at launch
Toolchain: rustc 1.97.0 (2d8144b78 2026-07-07), cargo 1.97.0 (c980f4866 2026-06-30), bun 1.3.14
Requested stages: fmt, typecheck, unit, parser-corpus, oracle, e2e, e2e-mysql80, browser, compose, bi-clients
Stages not requested: freshness (runs after banking, in the release chain's closing pass), recovery, soak (opt-in: hours, by design), memsoak (opt-in: hours, by design), bench, accept

| stage | verdict | minutes | note |
|---|---|---|---|
| fmt | PASS | 0.0 |  |
| typecheck | PASS | 0.1 |  |
| unit | PASS | 3.1 |  |
| parser-corpus | PASS | 0.1 |  |
| oracle | PASS | 0.2 |  |
| e2e | PASS | 7.1 |  |
| e2e-mysql80 | PASS | 7.2 |  |
| browser | PASS | 0.9 |  |
| compose | PASS | 3.7 |  |
| bi-clients | PASS | 1.3 |  |

## What this profile does not cover

Benchmark evidence is NOT regenerated: an rc ships the previous
stable release's numbers, so the freshness gate is not part of this
profile. Bank tests/e2e/results.md and results-mysql80.md on PASS.
The additional recovery fault matrix runs in the stable profile only.

The complete profile passed 1,039 unit tests (43 skipped), all 1,231 oracle
queries byte-exact against MySQL 8.4, and 5,754 E2E checks on each MySQL
version (44 skipped, 30 documented-gap warnings, zero failures per version).
Both E2E ledgers and the oracle ledger are banked alongside this report.

The uncommitted path at launch was the generated oracle ledger from an earlier
interrupted attempt. The driver shelved it before provenance-sensitive stages;
the engine sources were committed and unchanged throughout the successful run.
Earlier attempts were discarded after a missing browser dependency or overlapping
harness activity disrupted setup. The browser dependency was installed before
this complete rerun. This is an rc gate, not a stable or benchmark qualification.
