# Correlated-subquery memoization: after

Measured 2026-09-05, release build, macOS/arm64, in-process
(`crates/pintail-exec/tests/dependent_subquery_ratio.rs`, `PINTAIL_RATIO_ROWS=20000`),
the same sweep as `dependent-subquery-ratio.md` with the dependent memo in place.

| shape | distinct keys D | repeats per key | inner executions | total ms | ms per outer row | before (ms) | speedup |
|---|---:|---:|---:|---:|---:|---:|---:|
| scalar | 1000 | 20 | 1000 | 345 | 0.017 | 6475 | 19× |
| scalar | 100 | 200 | 100 | 65 | 0.003 | 6430 | 99× |
| scalar | 10 | 2000 | 10 | 37 | 0.002 | 6277 | 170× |
| scalar | 1 | 20000 | 1 | 34 | 0.002 | 6812 | 200× |
| exists | 1000 | 20 | 1000 | 201 | 0.010 | 3574 | 18× |
| exists | 100 | 200 | 100 | 52 | 0.003 | 3444 | 66× |
| exists | 10 | 2000 | 10 | 36 | 0.002 | 3489 | 97× |
| exists | 1 | 20000 | 1 | 37 | 0.002 | 3461 | 94× |
| in | 1000 | 20 | 1000 | 6244 | 0.312 | 130947 | 21× |
| in | 100 | 200 | 100 | 665 | 0.033 | 126985 | 191× |
| in | 10 | 2000 | 10 | 131 | 0.007 | 127459 | 973× |
| in | 1 | 20000 | 1 | 78 | 0.004 | 127324 | 1632× |

Inner executions now equal the number of distinct outer tuples exactly - the
test asserts that equality, so a wrong hit (fewer) or a missed share (more)
fails it. Every shape's floor at one key (34-78 ms) is the outer scan plus
one inner execution; that is what remains once repetition is gone.

The memo does not change the per-execution cost, so the case it does not
help is the one with no repetition: D = N, where every lookup misses and
the query pays a key build and a hash probe per row on top of what it paid
before. That overhead is bounded by the memo's entry cap and its memory
charge - at the cap or on a refused charge the memo drops itself and the
operator finishes exactly as it did before - and was not measurable in the
sweep's least-repeated column against the pre-memo baseline (345 ms vs
6475 ms is dominated by the 20-way sharing at D = 1000, not by overhead).

Correlated IN keeps its 20× per-execution premium over the other shapes
(0.312 vs 0.017 ms per row at D = 1000): its inner join is still planned
per execution. The memo shares those executions; it does not make each one
cheaper. That is the separate "plan once, execute per tuple" change.
