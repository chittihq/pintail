# Storage scan qualification

This change caches immutable block directories and reuses predicate columns
that are already decoded when the output projection is identical. It changes
no on-disk format. How much it helps depends on the scan shape. Wide
arithmetic is not expected to get faster from avoiding a small amount of
storage work.

## 0.1.7 release requalification

This evidence was re-run for the 0.1.7 stable release. The baseline is the
previous stable release, tag `v0.1.6` (tag object
`2e0939feec665b49ad3bdf0714916a1b0e63adb1`, commit
`596a934c5bfef63de4acad1e46806e152e79ecca`). The candidate is the 0.1.7
release tree (`938dd95a1d0f8f7ae56775e098fea8950c372f97`). Both releases
contain this change, so the pair measures what every other engine change
between the two releases did to it. That includes the 0.1.7 storage work:
segment format 7 (narrower dictionary indexes, no null bitmap for a block
without NULLs), positioned block reads behind one block cache, vector
decoding kernels, and a filter column that is also projected now being
decoded once.

**These numbers are not comparable with the 0.1.6 tables below**, which came
from 8-vCPU hosts. Every workload here ran on one 16-vCPU cloud host (AMD
Ryzen 9 9950X, 31 GiB of RAM, `/dev/kvm` present, so not a fallback VM) with
a local docker daemon, one workload after another: the probes, then the
TPC-H passes, then the 20M pair, then the format-7 supplement and the probe
repeat. Pintail and the probes ran natively; the harness MySQL, and in the
20M pair every engine, ran in containers. Compare the two arms only within
each table.

The executables are release builds from clean detached checkouts, built on
this host, each with its own target directory. Pintail digests are
`32438504…` (baseline) and `4d977aae…` (candidate); the probe digests are in
the [JSON](storage-scan-qualification.json). They are not the builds of the
[Q05 requalification](q05-join-qualification.md#017-release-requalification),
which ran on its own host. No other work of this study ran during a
measurement. The host did not start idle: it was still streaming its
restored disk in while the first probes ran, and its own management agents
kept between one and four logical CPUs busy at times during the baseline
adversarial and scan probes. The uniform probes were therefore repeated on
the quiet host at the end, both arms again; the repeat is the table of
record. The whole-host [load samples](storage-scan-qualification/rel-0.1.7-host-load.json),
taken every 15 seconds, show both periods. A first TPC-H attempt failed
before measuring anything because the docker daemon still wrote through the
retired disk mount; the daemon was restarted and the pass started again.

### Row preservation: 20 million adversarial rows

The fixture was seeded once by the `v0.1.6` probe and then opened by both
executables, baseline first. Both checked every returned value, and its
order, against independently generated expected rows, including a check that
no trailing rows were missing. The four answer files match byte for byte
(`cmp`), and their SHA-256 digests are identical to the original study's,
0.1.5's and 0.1.6's.

The fixture has the shape described in the 0.1.5 section: 39 segments of at
most 524,288 rows, a partial final segment, and 1,511,607,179 bytes in its
directory including the manifest and WAL. That is 2,439,848 bytes more than
the 0.1.6 study's copy, which `v0.1.5` seeded in segment format 4; `v0.1.6`
writes format 6, which adds the persisted side-index postings. Opening it
with the candidate changed nothing on disk: the file list and every file
size, the manifest's included, are identical before and after
([listing](storage-scan-qualification/rel-0.1.7-fixture-files.txt)), and the
baseline's repeat probes later read the uniform fixture again after the
candidate had opened it. The storage probes open the table directly and
start no background merge, so no segment was rewritten into format 7; the
candidate read the format-6 segments as they were. (A server is different:
the Q05 study saw the candidate merge a baseline-built replica into format 7
after start. The TPC-H and 20M runs here gave each arm its own replica.)

| Selection / projection | Returned rows | v0.1.6 decoded | 0.1.7 decoded | v0.1.6 pruned | 0.1.7 pruned |
|---|---:|---:|---:|---:|---:|
| Clustered text, predicate-only output | 632,501 | 1,221 | 1,221 | 3,648 | 3,648 |
| Clustered text, reordered mixed output | 632,501 | 1,529 | **1,452** | 22,816 | **18,024** |
| Null predicate, reordered mixed output | 822,359 | 6,105 | **4,884** | 18,240 | **14,592** |
| No matches | 0 | 1,221 | 1,221 | 23,124 | **18,255** |

The baseline reports exactly the counters of every earlier study. The
candidate's differ in the three cases whose output also contains the
predicate column, and that is the intended effect of a 0.1.7 change: a
filter column that is also projected is now taken from the predicate fetch,
compacted to the kept rows, instead of being decoded a second time. In the
clustered case the 77 output blocks of that column are no longer decoded
(1,529 − 77 = 1,452); in the null-predicate case all 1,221 of them are not
(6,105 − 1,221 = 4,884). The pruned counter no longer counts that column's
exclusions either, which removes 4,792, 3,648 and 4,869 from the three cases.
The answers do not change. Raw output:
[baseline](storage-scan-qualification/rel-0.1.7-adversarial-baseline.txt),
[candidate](storage-scan-qualification/rel-0.1.7-adversarial-candidate.txt),
[answer digests](storage-scan-qualification/rel-0.1.7-adversarial-answers.sha256).
Both arms used the example sources in their own trees, which are identical
between the two releases.

### Uniform 20-million-row probes

Both executables read the same fixture, which the `v0.1.6` probe seeded (20
segments, format 6). Each probe runs two warmups and seven measured
iterations, with SQL settled-result memoization disabled, and checks every
expected result. This is warm-cache evidence. In each round all baseline
probes ran before any candidate probe. The table is the repeat on the quiet
host:

| Probe | Case | v0.1.6 median ms | 0.1.7 median ms | v0.1.6 min ms | 0.1.7 min ms | Speedup | Decoded blocks, v0.1.6 → 0.1.7 |
|---|---|---:|---:|---:|---:|---:|---:|
| scan | narrow-last | 37.873 | 11.255 | 37.808 | 11.192 | 3.37× | 1,221 → 1,221 |
| scan | wide | 1,343.769 | 698.305 | 1,332.461 | 694.542 | 1.92× | 29,304 → 29,304 |
| scan | text-all | 23.136 | 10.166 | 23.007 | 9.968 | 2.28× | 1,221 → 1,221 |
| scan | text-selective | 22.963 | 10.325 | 22.920 | 10.107 | 2.22× | 1,221 → 1,221 |
| scan | mixed-selective | 119.280 | 62.896 | 118.620 | 62.473 | 1.90× | 3,663 → 2,442 |
| query | numeric-filter | 20.228 | 2.356 | 19.184 | 2.318 | 8.59× | — |
| query | text-filter | 18.346 | 1.619 | 17.747 | 1.489 | 11.33× | — |
| query | text-all | 25.000 | 1.849 | 24.781 | 1.758 | 13.52× | — |
| query | wide-expression | 12,863.205 | 8,506.138 | 12,802.667 | 8,456.330 | 1.51× | — |

Every result value is identical in both arms. The candidate is faster in
every case, on the median and the minimum. mixed-selective decodes one
column fewer for the reason given above. The dense text scans that 0.1.6
made about 23% slower are now 2.2–2.3× faster than `v0.1.6`. The three
count queries fall from about 20 ms to about 2 ms. This study did not
attribute any of it to a commit.

The first round, run while the host was still restoring its disk, agrees on
the candidate (every scan within 4% of the repeat, every query within 3%
except numeric-filter, 1.99 against 2.36 ms) but not on the baseline: its
wide scan took 1,598 ms against 1,344 ms in the repeat, narrow-last 42.8
against 37.9 ms, and text-all 25.4 against 23.1 ms. On those first-round
numbers the speedups are larger (wide 2.29×, narrow-last 3.68×) and are not
claimed. Raw output:
[scan baseline](storage-scan-qualification/rel-0.1.7-uniform-scan-baseline-repeat.txt),
[scan candidate](storage-scan-qualification/rel-0.1.7-uniform-scan-candidate-repeat.txt),
[query baseline](storage-scan-qualification/rel-0.1.7-uniform-query-baseline-repeat.txt),
[query candidate](storage-scan-qualification/rel-0.1.7-uniform-query-candidate-repeat.txt);
first round without the `-repeat` suffix.

### Segment format 7

The fixtures above were written by `v0.1.6`, so the candidate read format 6.
A supplement had the candidate seed both fixtures itself, in format 7, and
run its own probes on them.

- **Row preservation.** The candidate-seeded adversarial fixture returns the
  same four answer files: identical SHA-256 digests, and the same counters as
  the candidate's row above. It is 1,501,693,860 bytes, 0.66% smaller than
  the format-6 copy: its bulk is 64-byte opaque payloads, which the narrower
  format does not shrink. The uniform fixture shrinks from 153,845,369 to
  86,117,771 bytes (−44.0%).
- **Downgrade.** `v0.1.6` refuses a copy of the candidate-written fixture at
  open: the candidate also writes manifest version 4, and `v0.1.6` stops
  there (`CorruptManifest … unsupported format version`), as the 0.1.7
  upgrade notes say a downgrade will. The copy was deleted afterwards.
- **Speed.** Reading format 7 is slower than reading format 6 for the wide
  projections. With the candidate binary, two runs on each fixture (format
  6: the repeat plus a further scan run, and the first round plus the repeat
  for the queries):

| Case | Format 6 median ms | Format 7 median ms | Change |
|---|---:|---:|---:|
| scan wide | 698.305 / 730.385 | 832.081 / 781.918 | **+7% to +19%** |
| scan text-all | 10.166 / 9.956 | 10.469 / 10.372 | +2% to +5% |
| scan mixed-selective | 62.896 / 62.688 | 62.946 / 63.305 | −1% to +1% |
| scan narrow-last | 11.255 / 11.893 | 11.576 / 11.611 | within noise |
| query text-all | 1.798 / 1.849 | 2.102 / 2.141 | **+14% to +19%** (about 0.3 ms) |
| query wide-expression | 8,523.288 / 8,506.138 | 9,066.196 / 9,234.082 | **+6% to +9%** |

This study did not establish why the wide decode is slower on format 7. Even on format 7 the candidate's wide scan
is 1.6–1.7× faster than `v0.1.6` on format 6, and wide arithmetic 1.39–1.42×.
Raw: `storage-scan-qualification/rel-0.1.7-format7-*`.

### TPC-H SF1

The two binaries alternated for three passes each, baseline first, each
with its own tree's harness (the harness sources are identical between the
releases). Every pass loaded a fresh replica of 8,660,779 rows across eight
tables (6,000,749 line items). Each of the four supported queries ran once
per replica, in a fixed order, with settled-result memoization disabled. All
**24 comparisons were byte-exact against MySQL 8.4.11**. This is the
repository's four-query workload, not the full 22-query suite. Both arms used
a 16 GiB per-query and 32 GiB process spill allowance and a 4 GiB
query-memory ceiling. A watcher outside the repository raised the harness
MySQL's buffer pool to 4 GiB in every pass as soon as the final server
answered over TCP, while it counted no line-item rows, and read it back at
4 GiB until the container was removed after the queries
([watcher log](storage-scan-qualification/rel-0.1.7-tpch-buffer-pool.txt)).

| Query | v0.1.6 median ms | 0.1.7 median ms | Speedup | v0.1.6 samples ms | 0.1.7 samples ms |
|---|---:|---:|---:|---|---|
| q01-pricing-summary | 2,993 | 3,016 | **0.99×** | 2,950 / 3,103 / 2,993 | 3,016 / 3,366 / 2,781 |
| q03-shipping-priority | 136 | 64 | 2.13× | 131 / 153 / 136 | 75 / 64 / 62 |
| q05-local-supplier-volume | 238 | 131 | 1.82× | 215 / 291 / 238 | 148 / 131 / 115 |
| q10-returned-item-reporting | 272 | 184 | 1.48× | 244 / 289 / 272 | 184 / 187 / 155 |

q01 is unchanged: its candidate median is 0.8% above the baseline's, inside
both arms' spread (2,781–3,366 ms in the candidate), and its fastest sample
is the candidate's. The three join queries are 1.5–2.1× faster. Raw pass
reports are `storage-scan-qualification/rel-0.1.7-tpch-{baseline,candidate}-{1,2,3}.json`.
The command is the one in the 0.1.5 section below.

### Canonical eight-query benchmark

`bun run benchmark/run.ts` ran four times on this host from clean
checkouts, in the order baseline, candidate, candidate, baseline (host
fingerprint `4b800c7c…`). Each checkout was reset to its commit before each
run and left untouched during it. Each engine container was limited to 8 CPUs
and 8 GiB, Pintail's query ceiling was 4 GiB, and each query ran two warmups
followed by 15 measured iterations. Neither tree's banked MySQL reference
matched this host, so every run measured its own. All four gates passed with
exact answers, and both engines reported zero concurrency errors at every
client count. This pair is separate from the Q05 study's, which ran on
another host of the same type and measured the same direction and size.

Memo-disabled engine track, pooling both runs of each arm (30 samples each):

| Query | v0.1.6 run medians ms | 0.1.7 run medians ms | v0.1.6 pooled median | 0.1.7 pooled median | v0.1.6 pooled min | 0.1.7 pooled min | Pooled median change |
|---|---:|---:|---:|---:|---:|---:|---:|
| Q1: Full table count | 1.49 / 1.68 | 0.33 / 0.36 | 1.55 | 0.36 | 1.41 | 0.28 | −76.8% |
| Q2: Filtered count | 28.48 / 25.86 | 2.88 / 2.67 | 26.87 | 2.73 | 24.80 | 2.54 | −89.8% |
| Q3: Group by status | 85.60 / 86.63 | 10.37 / 11.17 | 85.73 | 10.73 | 82.37 | 9.43 | −87.5% |
| Q4: Region × status breakdown | 101.96 / 102.06 | 14.11 / 13.35 | 101.97 | 13.52 | 98.11 | 12.64 | −86.7% |
| Q5: Monthly revenue (2023) | 54.10 / 54.89 | 13.07 / 12.58 | 54.67 | 12.73 | 51.98 | 12.16 | −76.7% |
| Q6: Top 10 spenders | 279.98 / 281.51 | 21.48 / 23.35 | 280.11 | 22.55 | 268.90 | 18.99 | −92.0% |
| Q7: Regional analytics | 222.03 / 232.61 | 35.03 / 38.59 | 224.82 | 36.44 | 213.50 | 29.72 | −83.8% |
| Q8: Join users + orders | 163.16 / 161.34 | 16.24 / 15.48 | 163.09 | 16.04 | 155.23 | 14.68 | −90.2% |

**No query is slower in the candidate.** Every candidate run is faster than
every baseline run on every query, far beyond the run-to-run spread of the
same build (up to 11.3% between the two baseline runs and 9.2% between the
two candidate runs). The Q8 and Q1 regressions the 0.1.6 study reported are
gone. On the memo track every candidate pooled median is lower, by 64% to
81%. At 16 clients on the mixed Q2–Q8 workload, Pintail completed 30.2 and
30.5 queries per second in the baseline runs and 217.0 and 262.6 in the
candidate runs. The raw reports are the
[baseline](storage-scan-qualification/rel-0.1.7-eight-query-baseline.json),
[candidate](storage-scan-qualification/rel-0.1.7-eight-query-candidate.json),
[candidate repeat](storage-scan-qualification/rel-0.1.7-eight-query-candidate-repeat.json)
and [baseline repeat](storage-scan-qualification/rel-0.1.7-eight-query-baseline-repeat.json)
runs. The harness PASS covers exact answers and its memo-dashboard speed
threshold. It is not a cross-revision regression gate.

### Release gate

The release chain banked the correctness gate separately; it was not re-run
here. Its fixed MySQL oracle corpus passed at `d9c6210c` (2,586 cases against
MySQL 8.4.11), and the banked E2E ledgers record 7,061 checks passed with 0
failed, 6 documented-gap warnings and 49 skipped on both MySQL 8.4 and 8.0.
The commits after `d9c6210c` up to the candidate change no engine crate and
no benchmark harness source.

### Deviations from the original procedure

- The pair compares the previous stable release with this release, not this
  change with its parent.
- Every workload ran on one 16-vCPU, 31 GiB cloud host with a local docker
  daemon, not on the 8-vCPU hosts of 0.1.6.
- The uniform probes ran twice; the first round's baseline was slowed by the
  host's disk restore, and the quiet repeat is the table of record.
- The candidate's counters differ from every earlier study's in three
  adversarial cases, by design of a 0.1.7 change; the answers are identical.
- A format-7 supplement was added, because 0.1.7 changes the segment format
  and the release-to-release fixture is format 6.
- The 20M pair ran baseline, candidate, candidate, baseline, every run from a
  clean checkout; none shared a MySQL reference.
- A watcher outside the repository set the TPC-H buffer pool; it was
  confirmed on every pass. One attempt that failed before measuring, on a
  docker daemon error, was discarded.

## Skip condition

The directory records each block's half-open physical row interval. The
interval comes from the on-disk block row counts and is checked against the
segment row count. The reader validates that the requested ranges are
ordered and do not overlap. It skips ranges that end before a block, and
decodes the block exactly when the next range intersects it. If that range
starts at or beyond the block's end, every later range starts even farther
along and cannot intersect the block either. Predicate evaluation supplies
the selected ranges before any projected block is considered. The
adversarial comparisons in each requalification test that whole path, including ranges that
cross scan boundaries and partial blocks.

## Historical: 0.1.6 release requalification (superseded)

This section records the requalification for the 0.1.6 release, before the
0.1.7 requalification above. Its numbers describe those revisions and hosts
only.

This evidence was re-run for the 0.1.6 stable release. The baseline is the
previous stable release, tag `v0.1.5` (commit
`2d328f8e2ed5681c0189607cbea424867faa869b`). The candidate is the 0.1.6
release tree (`8b343370ca9741b94d04012c88ffdd6dd330e24e`). Both releases
contain this change and the later Q05 join change, so the pair measures what
every other engine change between the two releases did to them. That
includes the 0.1.6 storage work: the segment side index, turned on by
default, and its postings persisted in the segment file.

The probes and the TPC-H passes ran on one 8-vCPU cloud host (AMD Ryzen 9
9950X, 15 GiB of RAM) with a local docker daemon; Pintail and the probes ran
natively and the harness MySQL in a container. The 20M pair ran on a second
host of the same type. **These numbers are not comparable with the 0.1.5
tables below**, which came from a 32-logical-CPU build host and a separate
docker host. Compare the two arms only within each table.

The executables are release builds from clean detached checkouts, built on
the probe host itself: Pintail digests `51a35a88…` (baseline) and `f572fbb4…`
(candidate). They are not the byte-identical builds of the
[Q05 requalification](q05-join-qualification.md#historical-016-release-requalification-superseded),
which were built on its own host from the same commits. No other work of this
study ran on the probe host while it measured. The host's own management
agents did: the whole-host
[load samples](q05-join-qualification/rel-0.1.6-host-load.json), taken every
15 seconds, show them in brief bursts. One of about two logical CPUs was
sampled during the baseline scan probe, and one of about four during the
third baseline TPC-H pass, again while the third candidate pass was seeding.
Neither arm was re-run for them.

### Row preservation: 20 million adversarial rows

The fixture was seeded once by the `v0.1.5` probe and then opened by both
executables. Both checked every returned value, and its order, against
independently generated expected rows, including a check that no trailing
rows were missing. The four answer files match byte for byte (`cmp`). Their
SHA-256 digests are also identical to the original study's and to 0.1.5's.

The fixture is the one described in the 0.1.5 section: 39 segments of at most
524,288 rows, a partial final segment, and 1,509,167,331 bytes in its
directory including the manifest and WAL.

| Selection / projection | Returned rows | v0.1.5 decoded | 0.1.6 decoded | v0.1.5 pruned | 0.1.6 pruned |
|---|---:|---:|---:|---:|---:|
| Clustered text, predicate-only output | 632,501 | 1,221 | 1,221 | 3,648 | 3,648 |
| Clustered text, reordered mixed output | 632,501 | 1,529 | 1,529 | 22,816 | 22,816 |
| Null predicate, reordered mixed output | 822,359 | 6,105 | 6,105 | 18,240 | 18,240 |
| No matches | 0 | 1,221 | 1,221 | 23,124 | 23,124 |

Both arms report exactly the counters the 0.1.5 candidate reported. Raw
output:
[baseline](storage-scan-qualification/rel-0.1.6-adversarial-baseline.txt),
[candidate](storage-scan-qualification/rel-0.1.6-adversarial-candidate.txt),
[answer digests](storage-scan-qualification/rel-0.1.6-adversarial-answers.sha256).
Both arms used the example sources in their own trees, which are identical
between the two releases.

### Uniform 20-million-row probes

Both executables read the same fixture, which the `v0.1.5` probe seeded (20
segments). Each probe runs two warmups and seven measured iterations, with
SQL settled-result memoization disabled, and checks every expected result.
This is warm-cache evidence. All baseline probes, the adversarial one
included, ran before any candidate probe.

| Probe | Case | v0.1.5 median ms | 0.1.6 median ms | v0.1.5 min ms | 0.1.6 min ms | Speedup | Decoded blocks |
|---|---|---:|---:|---:|---:|---:|---:|
| scan | narrow-last | 39.182 | 38.481 | 39.018 | 38.453 | 1.02× | 1,221 |
| scan | wide | 1,396.763 | 1,392.174 | 1,386.530 | 1,373.371 | 1.00× | 29,304 |
| scan | text-all | 19.274 | 23.665 | 19.231 | 23.539 | **0.81×** | 1,221 |
| scan | text-selective | 19.257 | 23.591 | 19.234 | 23.520 | **0.82×** | 1,221 |
| scan | mixed-selective | 116.881 | 125.006 | 116.640 | 123.993 | **0.94×** | 3,663 |
| query | numeric-filter | 20.741 | 23.391 | 18.699 | 21.671 | **0.89×** | — |
| query | text-filter | 19.814 | 19.842 | 18.957 | 18.847 | 1.00× | — |
| query | text-all | 28.097 | 28.275 | 25.758 | 27.299 | 0.99× | — |
| query | wide-expression | 12,953.082 | 12,072.359 | 12,594.352 | 12,030.453 | 1.07× | — |

Decoded blocks are identical in both arms. **The dense text scans
(text-all and text-selective) are about 23% slower in 0.1.6, on the median
and the minimum alike**; the seven samples in each arm are tight, so this is
not sampling noise. mixed-selective is 7% slower and the numeric-filter query
13% slower on the median (16% on the minimum). The wide scan and the other
queries are unchanged, and wide arithmetic is 7% faster. The same blocks are
decoded, so the extra time is spent per block, not on more blocks. This study
did not attribute it to a commit. Raw output:
[scan baseline](storage-scan-qualification/rel-0.1.6-uniform-scan-baseline.txt),
[scan candidate](storage-scan-qualification/rel-0.1.6-uniform-scan-candidate.txt),
[query baseline](storage-scan-qualification/rel-0.1.6-uniform-query-baseline.txt),
[query candidate](storage-scan-qualification/rel-0.1.6-uniform-query-candidate.txt).

### TPC-H SF1

The two binaries alternated for three passes each. Every pass loaded a fresh
replica of 8,660,779 rows across eight tables (6,000,749 line items). Each of
the four supported queries ran once per replica, in a fixed order, with
settled-result memoization disabled. All **24 comparisons were byte-exact
against MySQL**. This is the repository's four-query workload, not the full
22-query suite. Both arms used a 16 GiB per-query and 32 GiB process spill
allowance and a 4 GiB query-memory ceiling. A watcher outside the repository
raised the harness MySQL's buffer pool to 4 GiB in every pass as soon as the
final server answered over TCP, while the server still counted no line-item
rows, and read it back at 4 GiB once the replica was ready, before the
queries.

| Query | v0.1.5 median ms | 0.1.6 median ms | Speedup | v0.1.5 samples ms | 0.1.6 samples ms |
|---|---:|---:|---:|---|---|
| q01-pricing-summary | 3,080 | 3,020 | 1.02× | 2,994 / 3,175 / 3,080 | 3,006 / 3,020 / 3,025 |
| q03-shipping-priority | 257 | 151 | 1.70× | 238 / 265 / 257 | 156 / 143 / 151 |
| q05-local-supplier-volume | 380 | 248 | 1.53× | 380 / 388 / 373 | 271 / 230 / 248 |
| q10-returned-item-reporting | 368 | 277 | 1.33× | 355 / 368 / 374 | 308 / 269 / 277 |

A first set of six passes ran with an earlier version of the watcher, which
could not show that it had reached the final MySQL server rather than the
image's initialization server. Those passes were also exact, but they are
discarded and not banked. The table above comes from the second set. Raw pass
reports are `storage-scan-qualification/rel-0.1.6-tpch-{baseline,candidate}-{1,2,3}.json`.
The command is the one in the 0.1.5 section below.

### Canonical eight-query benchmark

This is the same paired 20M run as in the
[Q05 requalification](q05-join-qualification.md#20m-paired-comparison-1), with
the same caveats: one 8-vCPU host with a local docker daemon, and four runs
in the order baseline, first candidate (sharing the baseline's MySQL
reference through the dirty-tree override), clean candidate, baseline repeat.
Each database container was limited to 8 CPUs and 8 GiB, Pintail's query
ceiling was 4 GiB, and each query ran two warmups followed by 15 measured
iterations. All four gates passed with exact answers, and both engines
reported zero concurrency errors in every run.

Memo-disabled engine track, pooling both runs of each arm (30 samples each):

| Query | v0.1.5 pooled median ms | 0.1.6 pooled median ms | v0.1.5 pooled min ms | 0.1.6 pooled min ms | Median speedup |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 1.83 | 2.08 | 1.47 | 1.81 | **0.88×** |
| Q2: Filtered count | 29.32 | 31.09 | 26.61 | 27.60 | 0.94× |
| Q3: Group by status | 96.20 | 98.75 | 89.68 | 89.85 | 0.97× |
| Q4: Region × status breakdown | 116.36 | 120.75 | 108.00 | 112.51 | 0.96× |
| Q5: Monthly revenue (2023) | 58.07 | 63.31 | 55.13 | 58.83 | 0.92× |
| Q6: Top 10 spenders | 298.91 | 308.45 | 285.11 | 292.39 | 0.97× |
| Q7: Regional analytics | 250.71 | 254.12 | 232.51 | 239.41 | 0.99× |
| Q8: Join users + orders | 182.34 | 213.67 | 162.11 | 183.36 | **0.85×** |

**Q8 (+17.2% pooled median, +13.1% pooled minimum) regressed between the
releases**, and every candidate run is slower than every baseline run. Q1 is
slower too (+13.6% median, +23.1% minimum), by about 0.3 ms absolute. Q5 is
slower by 9.0% on the median and 6.7% on the minimum. The remaining queries
moved less than the two baseline runs of the same build differ from each
other (up to 13.9% on a median). Taken alone, the clean pair shows every query
but Q7 10–23% slower on the median; the baseline repeat shows how much of
that one pair is this host's noise. This storage change is not established as
the cause of any of these; the pair spans the whole release. The raw reports
are in the Q05 raw directory:
[baseline](q05-join-qualification/rel-0.1.6-eight-query-baseline.json),
[candidate](q05-join-qualification/rel-0.1.6-eight-query-candidate.json),
[first candidate run](q05-join-qualification/rel-0.1.6-eight-query-candidate-shared-reference.json),
[baseline repeat](q05-join-qualification/rel-0.1.6-eight-query-baseline-repeat.json).

### Release gate

The release chain banked the correctness gate separately; it was not re-run
here. Its validation at `374d7510` passed all stages, including 1,948 oracle
cases byte-exact against MySQL 8.4 and 7,061 E2E checks with 0 failures (6
warnings, 49 skipped) on MySQL 8.4 and 8.0. Commits after `374d7510` up to the
candidate only bank evidence.

### Deviations from the original procedure

- The pair compares the previous stable release with this release, not this
  change with its parent.
- The probes, the TPC-H passes and the 20M pair ran on 8-vCPU, 15 GiB cloud
  hosts with local docker daemons, not on the 32-logical-CPU build host and
  separate docker host of 0.1.5. The binaries here were built on the probe
  host and differ in digest from the Q05 study's builds of the same commits.
- Both arms used their own tree's probe sources; no copying was needed.
- A watcher outside the repository set the TPC-H buffer pool; it was
  confirmed on every pass. One earlier set of passes was discarded, as
  described above.
- The 20M pair was run four times rather than twice; the first candidate run
  used the dirty-tree override to share the baseline's MySQL reference.

## Historical: 0.1.5 release requalification (superseded)

This section records the requalification for the 0.1.5 release, before the
0.1.6 requalification above. Its numbers describe those revisions and hosts
only.

This evidence was re-run for the 0.1.5 stable release. The baseline is the
previous stable release, tag `v0.1.4`
(`a36e18abff0c9344a0dc14d30f159aac081b272e`). The candidate is the 0.1.5
release tree (`c7fde2b7cb2fdf473340d21c30924645a21899ec`). `v0.1.4` branched
from `a05e9e54`, which is this change's original baseline, and contains
neither this change (`2dc08de1`) nor the later Q05 join change. The pair
therefore spans both changes and every other engine change between the two
releases.

Both release executables are the same builds used in the
[Q05 requalification](q05-join-qualification.md#historical-015-release-requalification-superseded).
Pintail and the storage probes ran natively on the build host, which has 32
logical CPUs and 30 GiB of RAM. The harness MySQL and the 20M benchmark
engines ran on a separate docker host with 16 logical CPUs and 60 GiB of RAM.
**Neither host was idle.** Ten unrelated long-running containers ran on the
docker host throughout. On the build host, a server process left behind by an
earlier, unrelated validation run used close to one logical CPU (82.8% on
average in the samples where it appeared). This study did not create that
process and did not stop it. No other harness or build ran during
measurement. Whole-host
[load samples](q05-join-qualification/rel-0.1.5-host-load.json) were taken
every 15 seconds and include this study's own work.

### Row preservation: 20 million adversarial rows

The fixture was seeded once by the `v0.1.4` probe and then opened by both
executables. Both checked every returned value, and its order, against
independently generated expected rows, including a check that no trailing
rows were missing. The four answer files match byte for byte (`cmp`). Their
SHA-256 digests are also identical to the original study's.

The fixture has 39 segments. Its directory totals 1,509,167,019 bytes,
including the manifest and WAL. Each segment holds at most 524,288 rows.
Matching text is clustered in one block out of every 32, with extra matches
at scan boundaries. The predicate column has one all-null block out of every
32 and scattered nulls every 97 rows. The projected binary and variable-width
text columns have nulls every 13 and 17 rows respectively. Each non-null
binary value holds 64 deterministically generated opaque bytes. The final
segment is partial.

| Selection / projection | Returned rows | v0.1.4 decoded | 0.1.5 decoded | v0.1.4 pruned | 0.1.5 pruned |
|---|---:|---:|---:|---:|---:|
| Clustered text, predicate-only output | 632,501 | 1,298 | 1,221 | 8,440 | 3,648 |
| Clustered text, reordered mixed output | 632,501 | 1,529 | 1,529 | 22,816 | 22,816 |
| Null predicate, reordered mixed output | 822,359 | 6,105 | 6,105 | 18,240 | 18,240 |
| No matches | 0 | 1,221 | 1,221 | 23,124 | 23,124 |

The counters are identical to the original study's in both arms. In the
clustered mixed projection, the 1,529 decodes are all 1,221 predicate blocks
plus 77 blocks for each of the four output columns. So 1,144 of the 1,221
blocks in each output column are skipped, while every expected matching row
is kept. The null predicate touches every block because its nulls are
scattered.

These are the scanner's cumulative physical block counters, not counts of
distinct blocks. Pruned counts include exclusions from repeated sliced reads,
so they must not be read as a percentage of the fixture's blocks.

**Probe source.** The `v0.1.4` store API predates the range wrapper that the
current examples return. The baseline therefore used the probe sources as of
`2dc08de1`, the original probe revision, copied into the `v0.1.4` checkout
and removed after the build. The candidate used the examples in its own tree.
The only difference between the two sources is the return type of the
predicate range. The fixture, predicates, projections and checks are
identical. Raw output:
[baseline](storage-scan-qualification/rel-0.1.5-adversarial-baseline.txt),
[candidate](storage-scan-qualification/rel-0.1.5-adversarial-candidate.txt),
[answer digests](storage-scan-qualification/rel-0.1.5-adversarial-answers.sha256).

### Uniform 20-million-row probes

Both executables read the same fixture, which the `v0.1.4` probe seeded (20
segments). Each probe runs two warmups and seven measured iterations, with
SQL settled-result memoization disabled, and checks every expected result.
This is warm-cache evidence. All baseline probes ran before any candidate
probe.

| Probe | Case | v0.1.4 median ms | 0.1.5 median ms | Speedup | Decoded blocks, v0.1.4 → 0.1.5 |
|---|---|---:|---:|---:|---:|
| scan | narrow-last | 70.893 | 36.092 | 1.96× | 1,221 → 1,221 |
| scan | wide | 1,429.146 | 1,425.426 | 1.00× | 29,304 → 29,304 |
| scan | text-all | 108.359 | 18.496 | 5.86× | 2,442 → 1,221 |
| scan | text-selective | 129.164 | 17.700 | 7.30× | 2,442 → 1,221 |
| scan | mixed-selective | 201.972 | 129.725 | 1.56× | 3,663 → 3,663 |
| query | numeric-filter | 31.901 | 25.003 | 1.28× | — |
| query | text-filter | 29.588 | 24.239 | 1.22× | — |
| query | text-all | 39.076 | 34.191 | 1.14× | — |
| query | wide-expression | 18,388.322 | 14,969.474 | 1.23× | — |

The wide projected scan is unchanged. Wide arithmetic is 1.23× faster in this
release pair. That gain comes from other changes between the releases; this
storage change did not produce it (the original study measured 1.02×). The
build host is not the machine the original probes ran on, so compare absolute
times only within this table. Raw output:
[scan baseline](storage-scan-qualification/rel-0.1.5-uniform-scan-baseline.txt),
[scan candidate](storage-scan-qualification/rel-0.1.5-uniform-scan-candidate.txt),
[query baseline](storage-scan-qualification/rel-0.1.5-uniform-query-baseline.txt),
[query candidate](storage-scan-qualification/rel-0.1.5-uniform-query-candidate.txt).

### TPC-H SF1

The two binaries alternated for three passes each. Every pass loaded a fresh
replica of 8,660,779 rows across eight tables (6,000,749 line items). Each of
the four supported queries ran once per replica, in a fixed order, with
settled-result memoization disabled. All **24 comparisons were byte-exact
against MySQL**. This is the repository's four-query workload, not the full
22-query suite. Both arms used a 16 GiB per-query and 32 GiB process spill
allowance and a 4 GiB query-memory ceiling. The harness MySQL's buffer pool
was raised to 4 GiB before seeding in every pass and was confirmed before
the queries ran.

| Query | v0.1.4 median ms | 0.1.5 median ms | Speedup | v0.1.4 samples ms | 0.1.5 samples ms |
|---|---:|---:|---:|---|---|
| q01-pricing-summary | 5,213 | 3,563 | 1.46× | 5,213 / 5,245 / 5,170 | 3,541 / 3,597 / 3,563 |
| q03-shipping-priority | 3,462 | 266 | 13.02× | 3,462 / 3,449 / 3,464 | 281 / 263 / 266 |
| q05-local-supplier-volume | 31,731 | 322 | 98.54× | 31,691 / 31,950 / 31,731 | 327 / 320 / 322 |
| q10-returned-item-reporting | 2,532 | 331 | 7.65× | 2,515 / 2,595 / 2,532 | 331 / 338 / 325 |

Run `PINTAIL_BENCHMARK_BINARY=<binary> bun run benchmark/run-tpch.ts --profile sf1`
with the spill and memo variables below. A watcher outside the repository
raised the buffer pool on each harness MySQL container, because the harness
does not set it. Raw pass reports are
`storage-scan-qualification/rel-0.1.5-tpch-{baseline,candidate}-{1,2,3}.json`.

```sh
export PINTAIL_DISABLE_SETTLED_MEMO=1
export PINTAIL_QUERY_SPILL_LIMIT_BYTES=17179869184
export PINTAIL_GLOBAL_SPILL_LIMIT_BYTES=34359738368
PINTAIL_BENCHMARK_BINARY=/path/to/binary bun run benchmark/run-tpch.ts --profile sf1
```

Set `innodb_buffer_pool_size=4294967296` on the harness-created MySQL source
before the measured queries.

### Canonical eight-query benchmark

This is the same paired 20M run as in the
[Q05 requalification](q05-join-qualification.md#20m-paired-comparison-2), with
the same caveats: a shared docker host and one discarded out-of-disk
candidate attempt. Both runs used 20 million synthetic rows on the same
docker host (fingerprint `2c89ea59…`). Each database container was limited to
8 CPUs and 8 GiB, Pintail's query ceiling was 4 GiB, and each query ran two
warmups followed by 15 measured iterations. Both gates passed with exact
answers, and both engines reported zero concurrency errors.

Memo-disabled engine track:

| Query | v0.1.4 median ms | 0.1.5 median ms | v0.1.4 min ms | 0.1.5 min ms | Median speedup |
|---|---:|---:|---:|---:|---:|
| Q1: Full table count | 4.59 | 2.37 | 4.41 | 2.34 | 1.94× |
| Q2: Filtered count | 51.32 | 42.19 | 50.36 | 39.78 | 1.22× |
| Q3: Group by status | 132.28 | 182.99 | 127.29 | 164.43 | **0.72×** |
| Q4: Region × status breakdown | 160.28 | 188.55 | 145.99 | 168.01 | **0.85×** |
| Q5: Monthly revenue (2023) | 98.39 | 102.37 | 92.89 | 94.27 | 0.96× |
| Q6: Top 10 spenders | 448.47 | 477.44 | 438.88 | 457.46 | **0.94×** |
| Q7: Regional analytics | 410.91 | 432.56 | 395.11 | 392.69 | **0.95×** |
| Q8: Join users + orders | 376.18 | 309.63 | 353.08 | 259.99 | 1.21× |

**Q3 (+38.3% median, +29.2% minimum) and Q4 (+17.6% median, +15.1% minimum)
regressed between the releases.** Both are larger than the repository's ±10%
noise allowance, and the minimums moved with the medians. The release chain's
own benchmark of the same candidate tree measured the same direction. Q6
(+6.5% median, +4.2% minimum) and Q7 (+5.3% median, minimum unchanged) are
also slower by more than 5%. This storage change is not established as the
cause of any of these; the pair spans the whole release. The raw reports are
in the Q05 raw directory:
[baseline](q05-join-qualification/rel-0.1.5-eight-query-baseline.json) and
[candidate](q05-join-qualification/rel-0.1.5-eight-query-candidate.json).

### Release gate

The release chain banked the correctness gate separately; it was not re-run
here. The stable-profile run at `6bf9a853` passed all stages, including 1,908
oracle cases byte-exact against MySQL 8.4 and 7,015 E2E checks with 0
failures (29 warnings, 44 skipped) on MySQL 8.4 and 8.0. Commits after
`6bf9a853` up to the candidate only bank evidence.

### Deviations from the original procedure

- The pair compares the previous stable release with this release, not this
  change with its parent.
- The probes and Pintail ran on a 32-logical-CPU build host, and neither host
  was idle.
- The baseline probe sources are the `2dc08de1` versions, unmodified;
  the candidate's differ only in the range return type.
- A watcher outside the repository set the TPC-H buffer pool; it was confirmed
  on every pass.

## Historical: original change qualification (superseded)

This section records the change study at candidate
`2dc08de1c8f8901544a1e5748e74598faa163bc0` against baseline `a05e9e54`,
before the 0.1.5 requalification. Its numbers describe those revisions only.
It ran on an idle 16-logical-CPU measurement machine.

- Adversarial fixture: the same counters as the 0.1.5 table above in both arms,
  and answer digests identical to this requalification's.
- Uniform probes: text-all and text-selective scans ran 4.84× and 5.67×
  faster, narrow-last 1.98×, mixed-selective 1.47× and wide 1.05×;
  scan-bound SQL ran 1.26–1.51× faster and wide arithmetic 1.02×. Raw:
  `storage-scan-qualification/uniform-{scan,query}-{baseline,candidate}.txt`.
- TPC-H SF1: three alternating passes with 16 GiB/32 GiB spill. Medians
  changed by between −0.36% and +0.73%: q01 9,252 → 9,219 ms, q03
  5,395 → 5,421 ms, q05 47,791 → 47,793 ms, q10 3,985 → 4,014 ms. Raw:
  `storage-scan-qualification/tpch-{baseline,candidate}-{1,2,3}.json`.
- Eight-query engine track: the largest median slowdown was Q4 at 5.8%, and
  its minimum improved. Raw:
  `storage-scan-qualification/eight-query-{baseline,candidate}.json`. One
  earlier baseline attempt was discarded after a separate native benchmark
  started during its timed queries.
- The rc gate passed at `2dc08de1` ([report](storage-scan-rc.md)).

**Capacity disclosure (historical, still true of `v0.1.4`).** The unchanged
baseline failed Q05 with the default 1 GiB query spill quota. That
[failed result](storage-scan-qualification/tpch-baseline-default-quota.json)
remains banked. The
[Q05 spill disclosure](q05-join-qualification.md#historical-q05-spill-disclosure)
explains the later fix. The tested spill allowance does not measure peak
usage or the minimum quota required.

## Reproduction

Build `storage_adversarial_probe`, `storage_scan_probe` (pintail-store) and
`storage_query_probe` (pintail-exec) for both arms. If the baseline tree does
not have them, copy the example sources into a detached baseline checkout
before building. Keep the resulting executables separate. Seed with the
baseline, and run every baseline probe before any candidate probe:

```sh
baseline-adversarial DATA 20000000 baseline-answers
candidate-adversarial DATA 20000000 candidate-answers
for n in 0 1 2 3; do
  cmp "baseline-answers/case-$n.txt" "candidate-answers/case-$n.txt"
done
baseline-scan UNIFORM 20000000 --seed-only
PINTAIL_DISABLE_SETTLED_MEMO=1 baseline-scan UNIFORM 20000000
PINTAIL_DISABLE_SETTLED_MEMO=1 baseline-query UNIFORM 20000000
# then the candidate scan and query probes on the same UNIFORM directory
```

## Banked evidence and scope

The [machine-readable comparison](storage-scan-qualification.json) keeps the
0.1.6 and 0.1.5 requalifications under `previous_requalifications` and the
original study under `historical`. The raw runs for this requalification
carry the `rel-0.1.7-` prefix in [the raw directory](storage-scan-qualification/),
including the 20M reports, and the earlier files remain. The freshness
registry tracks this qualification against the engine crates and the
benchmark harness inputs.

The supported claim is narrow. Dense text-predicate scans decode half as many
blocks as before this change, and still do in 0.1.7. The sparse, nullable
fixture returns identical answers while skipping most projected blocks,
whether `v0.1.6` or 0.1.7 wrote it. 0.1.7 answers every TPC-H and 20M
benchmark query exactly. Against `v0.1.6` it is faster on every uniform probe
(1.9–13.5×), on every 20M engine-track query (4–12× pooled median), and on
three of the four TPC-H queries, with q01 unchanged. The 0.1.6 regressions in
the dense text scans and the 20M Q8 and Q1 queries are gone. One cost is
reported as such: reading its own format 7, the candidate's wide scan is
7–19% slower and wide arithmetic 6–9% slower than reading format 6.
