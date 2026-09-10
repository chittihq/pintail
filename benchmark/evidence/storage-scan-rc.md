# Validation report — 2026-09-09T18:42:10.649Z

Verdict: **PASS**
Profile: **rc** — complete
Claim when complete: rc correctness gates passed, on both MySQL majors the release claims to cover
HEAD: 26f4c9f2 test(store): qualify sparse nullable block selection against a baseline — clean tree
Toolchain: rustc 1.97.0 (2d8144b78 2026-07-07), cargo 1.97.0 (c980f4866 2026-06-30), bun 1.3.14
Requested stages: fmt, typecheck, unit, parser-corpus, oracle, e2e, e2e-mysql80, browser, compose, bi-clients
Stages not requested: freshness (runs after banking, in the release chain's closing pass), recovery, soak (opt-in: hours, by design), memsoak (opt-in: hours, by design), bench, accept

| stage | verdict | minutes | note |
|---|---|---|---|
| fmt | PASS | 0.0 |  |
| typecheck | PASS | 0.1 |  |
| unit | PASS | 2.5 |  |
| parser-corpus | PASS | 0.1 |  |
| oracle | PASS | 0.2 |  |
| e2e | PASS | 7.0 |  |
| e2e-mysql80 | PASS | 7.2 |  |
| browser | PASS | 0.9 |  |
| compose | PASS | 2.2 |  |
| bi-clients | PASS | 1.5 |  |

## What this profile does not cover

Benchmark evidence is NOT regenerated: an rc ships the previous
stable release's numbers, so the freshness gate is not part of this
profile. Bank tests/e2e/results.md and results-mysql80.md on PASS.
The additional recovery fault matrix runs in the stable profile only.

The initial run failed because the fresh sync lacked the generated Prisma client.
After running `bun run generate:prisma` in `tests/e2e`, the complete profile
was rerun and passed. Both E2E ledgers have 5,754 passing checks, 44 skips,
and 30 warnings; no new warning category compared with the baseline.
The unit stage passed 1,034 tests with 43 skipped. All 1,231 oracle queries
matched MySQL 8.4 byte-for-byte.
