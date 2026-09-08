# Validation report — 2026-09-08T19:37:56.292Z

Verdict: **PASS**
Profile: **rc** — complete
Claim when complete: rc correctness gates passed, on both MySQL majors the release claims to cover
HEAD: cc165c3 merge(snapshot): reconcile throughput experiment ancestry — clean tree
Toolchain: rustc 1.97.0 (2d8144b78 2026-07-07), cargo 1.97.0 (c980f4866 2026-06-30), bun 1.3.14
Requested stages: fmt, typecheck, unit, oracle, e2e, e2e-mysql80, browser, compose, bi-clients
Stages not requested: freshness (runs after banking, in the release chain's closing pass), recovery, soak (opt-in: hours, by design), memsoak (opt-in: hours, by design), bench, accept

| stage | verdict | minutes | note |
|---|---|---|---|
| fmt | PASS | 0.0 |  |
| typecheck | PASS | 0.1 |  |
| unit | PASS | 2.0 |  |
| oracle | PASS | 0.2 |  |
| e2e | PASS | 7.0 |  |
| e2e-mysql80 | PASS | 7.2 |  |
| browser | PASS | 0.8 |  |
| compose | PASS | 2.0 |  |
| bi-clients | PASS | 0.7 |  |

## What this profile does not cover

Benchmark evidence is NOT regenerated: an rc ships the previous
stable release's numbers, so the freshness gate is not part of this
profile. Bank tests/e2e/results.md and results-mysql80.md on PASS.
The additional recovery fault matrix runs in the stable profile only.

This file banks the report only. The original report and logs are in
`validate-out/runs/2026-09-08T19-19-12-740Z-rc/`.
