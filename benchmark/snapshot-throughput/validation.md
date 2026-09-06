# Validation report — 2026-09-06T16:09:56.825Z

Verdict: **PASS**
Profile: **development** — complete
Claim when complete: the code compiles, lints, typechecks and passes its unit tests
HEAD: a2b28a9 perf(snapshot): measure worker parallelism and composite-key seeks — clean tree
Toolchain: rustc 1.97.0 (2d8144b78 2026-07-07), cargo 1.97.0 (c980f4866 2026-06-30), bun 1.3.14
Requested stages: fmt, typecheck, unit
Stages not requested: freshness (runs after banking, in the release chain's closing pass), oracle, e2e, e2e-mysql80, recovery, soak (opt-in: hours, by design), memsoak (opt-in: hours, by design), browser, compose, bench, accept

| stage | verdict | minutes | note |
|---|---|---|---|
| fmt | PASS | 0.0 |  |
| typecheck | PASS | 0.1 |  |
| unit | PASS | 12.0 |  |

## What this profile does not cover

No differential gate: nothing here compares Pintail against MySQL.
No measured evidence: benchmark and acceptance numbers are untouched.

Per-stage logs sit next to this report; crashed-container logs are
captured as <stage>-containers.log before harness cleanup removes them.
