# Biomimetic experiment audit — 2026-08-15

This is the controlling evidence ledger for e30-e79. Older prose in
`RESULTS.md` is historical context; it cannot promote an idea when this ledger
marks the experiment invalid or rejected.

## What was actually executed

All 50 packages were executed locally in one continuous audit run:

- workspace unit tests: pass;
- workspace Clippy with `-D warnings`: pass;
- release binaries e30 through e79: all exited successfully.

Successful execution proves only that a harness runs. It does not prove that
the harness measures its claim. Seventeen challenged experiments were rebuilt
and re-audited at the implementation level. The other 33 were read line by
line after execution, with decisions, labels, budgets, checksums, benchmark
closures, controls, and claimed metrics traced to their sources.

The three surviving prototypes (e39, e52, e61) also reproduced in the pinned
Linux container. No other positive claim is reinstated by this audit.

## Verdict ledger

| ID | Audit status | Finding |
|---|---|---|
| e30 | invalid positive | Fixed column IDs masquerade as a frequency baseline, bundle capacity is unequal, and “recovery” also fires without a shift. |
| e31 | reject retained | Discounted UCB beats pheromone work and recovery; the reported recovery field is unreliable for policies that never recover. |
| e32 | reject retained | Equal pivot bytes are enforced, but remodeling loses p95 and churns more than frequency. |
| e33 | reject retained | Exact row counts are checked; the hierarchy does not clear elapsed and hostile-data gates. |
| e34 | reject retained | Root scoring consumes counterfactual histogram knowledge and still loses to a full index and simple frequency. |
| e35 | invalid positive | The learner is handed the exact half-run phase boundary and opens a fresh model bank there. |
| e36 | rebuilt; reject | Label-free detector exactly ties simpler diagonal distance. |
| e37 | reject retained; contaminated metric | Per-phase model banks use the known half-run boundary; affinity still loses to fixed buckets. |
| e38 | reject retained | Queue/CDC work drains exactly, but fever worsens query p99 versus the controls. |
| e39 | **candidate** | Real byte corruption, discovery, fail-loud overlap, exact disjoint answers, and 97.3% fewer verified bytes; PTSEG persistence is still required. |
| e40 | reject retained | Charged model loses the preregistered work and p95 gates. |
| e41 | reject retained; false shift | Adding four modulo eight to a uniform dashboard leaves the distribution unchanged; the candidate already loses to GreedyDual. |
| e42 | reject retained | Real residual reconstruction is exact, but only seasonal data shrinks (11.8%) and decoding is slower. |
| e43 | rebuilt; reject | Observable benchmark state reverses the old elapsed claim; stable modeled work regresses and the adaptive loop is 1.9-3.9x slower. |
| e44 | rebuilt; reject | Actual Top-K/payload work falls back safely, but the uncorrelated control is 11.8% slower. |
| e45 | reject retained | EWMA dominates; the “unbounded” control also compounds a ratio until its q-error saturates, so it is not useful evidence. |
| e46 | reject retained | Exact read versions agree, while quorum work is 4.9-48x worse. |
| e47 | rebuilt; reject | Observable morsel completion changes by less than 1%. |
| e48 | invalid positive | No file read occurs; “latency” is a hand-written equation and the candidate is interval unioning. |
| e49 | rebuilt; reject | Real slot scheduling shows SJF has 4.4-5.5x lower p95 slowdown. |
| e50 | reject retained | Conserved queue work drains, but the coupled controller does not beat debt-only control. |
| e51 | invalid positive | The claimed phase shift adds three modulo six to a uniform source, producing the same distribution; recovery-on-first-hit is not recovery. |
| e52 | **candidate** | Real FK arrays, exact join answers, actual demand-build scan, 55-78% input reduction, and 40-byte metadata; PTSEG lineage remains. |
| e53 | invalid positive | All work, build, and storage figures are formulas; no aggregate, partial, lineage, or invalidation is executed. |
| e54 | reject retained | Evaporation raises read work and misses the write margin. |
| e55 | reject retained | Charged prefetch model loses to fixed depth, including 6.8x cost under pressure. |
| e56 | rebuilt; reject | Equal caps and observed utility still tie static weights on the stable control. |
| e57 | invalid positive | ATP prediction calls the exact same function as “true cost,” so 100% accuracy is an identity. |
| e58 | rebuilt; reject | Real chunks/checksums require 6-7 probes and cannot repay on the unchanged-filter control. |
| e59 | invalid positive | The candidate receives current ground-truth utility, and a synthetic storm penalty is applied only to one baseline. |
| e60 | rebuilt; reject | Real buffers cut allocations but fail churn-slack and fragmentation gates. |
| e61 | **candidate** | Decisions accept observable signals only; exact preserved answers, zero false aborts, spill preservation, and early containment reproduce on Linux; real telemetry calibration remains. |
| e62 | rebuilt; reject | Exact sharded/global aggregation gains only 3.8% under shift and costs 25% on tiny work. |
| e63 | invalid positive | Multiple protected tasks effectively receive the same reserve, making protected spill zero by construction. |
| e64 | rebuilt; reject | Bounded forecasting ties or loses to a full-slack reactive controller. |
| e65 | reject retained | It fails the strongest control and population-amplitude gates. |
| e66 | reject retained | Dynamic niches range from -8.3% to +5.5% versus global GreedyDual with much higher loop cost. |
| e67 | reject retained | Heat-only tiers dominate and succession makes thousands of needless transitions. |
| e68 | reject retained | Oracle labels are used only to score detection, not decide; periodic probing still wins the one-reversal control. |
| e69 | rebuilt; reject | Real queues expose legitimate-flash p99 regression from 16 to 38. |
| e70 | rebuilt; reject | Equal full budgets collapse the old 2.56x claim; auctions worsen makespan 6.1-13.1%. |
| e71 | invalid decision; reject | The germination decision consumes the ground-truth `stable` label and still misses the p95 gate. |
| e72 | rebuilt; reject | Distinct 24-bit columns remove aliasing; gains of 18.4-19.4% miss the 20% gate. |
| e73 | invalid positive | “Symbiotic” admission is `family < 16`; its net-reuse field does not control admission. |
| e74 | reject retained | No reset fires; the recovery counter is not a robust phase metric, but the negative verdict is unchanged. |
| e75 | reject retained | Actual XOR repair is exact for one loss, but reads more than the modeled restore and is slower. |
| e76 | rebuilt; reject | Persisted reconciliation is exact and sparse-efficient but 9.9% slower than flat checksums at dense drift. |
| e77 | invalid positive; reject | The candidate loses to heat-only on decoded rows and worsens p95 versus fixed granules. |
| e78 | rebuilt; reject | Independent routing/hashing makes the receptor ensemble produce 3.16x the false reads of learned allocation. |
| e79 | reject retained | Triggered probes are exact and safe but miss the 25% improvement gate. |

## Candidate set

Only three ideas justify a next experiment:

1. **e39 — persisted corruption quarantine:** isolate known-bad immutable
   granules so disjoint reads remain available.
2. **e52 — demand-grown join fibers:** build tiny per-segment FK reachability
   only after repeat demand repays the build scan.
3. **e61 — consensus runaway containment:** abort only when independent
   observable signals agree, while diverting recoverable pressure to spill.

These are prototype findings, not engine claims and not claims of external
novelty. Each still requires the real Pintail integration named in its row.

## Evidence contract for future experiments

An experiment cannot advance unless all of the following are true:

1. The checksum is derived from the actual logical answer, not an experiment ID
   or an input token alone.
2. Decision code cannot receive truth labels, phase boundaries, future costs,
   or hindsight statistics. Oracle data may be used only after the decision to
   score it.
3. Every competitor receives the same capacity, budget, input, and accounting;
   the strongest simple control must be included.
4. Claimed CPU, I/O, bytes, memory, and latency are observed from executed work.
   A calibrated model must be labeled as a model and cannot establish an engine
   performance claim.
5. Benchmark closures make the full policy outcome observable so policy work
   cannot be removed by dead-code elimination.
6. At least one hostile control targets the mechanism's likely failure mode.
7. A positive result must reproduce on both declared targets with raw evidence,
   environment fingerprints, and no unexplained target-specific regression.
