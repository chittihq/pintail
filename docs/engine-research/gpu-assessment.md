# Would a GPU path pay?

Assessment date: 2026-10-02. No code accompanies this; it is a decision
record. Figures are from the 20-million-row benchmark replica on an
8-core machine (see `benchmark/`), result memo off.

## Where the engine stands

The analytical statements of the benchmark finish in 6 to 60 ms of wall
clock and 20 to 350 ms of CPU across eight cores. A grouped aggregate over
two columns of the 20-million-row table reads about 60 MB of stored blocks
(bit-packed integers, dictionary indexes, LZ4 where it saves space),
decodes 40 million values and folds them in roughly 25 ms. That is 1.6
billion values per second, and the time is spread over unpacking, folding
and group bookkeeping with no single stage above a quarter of it.

## What a GPU path would have to carry

**The data has to reach the device, per query or once.** A discrete
device sits behind a bus that moves 10 to 25 GB/s in practice. Sending the
60 MB of stored bytes costs 3 to 6 ms, a fifth of the whole query, before
any work is done; sending decoded columns (240 MB for the same statement)
costs 10 to 25 ms, all of it. So blocks must go over still encoded and be
unpacked on the device, or live there already. Unpacking fixed-width
bit-packed integers on a device is straightforward. LZ4 is not: each match
depends on bytes just produced, so one block is a sequential job, and the
only parallelism is one block per thread of the device, which is the
parallelism eight cores already have.

Keeping columns resident on the device removes the transfer and adds a
second copy of the hot data in memory that is scarcer and costlier than
RAM, to be kept exact under change capture: every flush, compaction and
recopy replaces files, and the memtable overlay (rows newer than any file)
must be applied on the host or shipped per query. The block cache has the
same invalidation problem on the host and solves it by file identity; a
device cache would need the same, plus eviction when another query's
working set does not fit.

**Semantics have to be exact on the device.** Answers must equal MySQL's
byte for byte. Integer and scaled-decimal sums are easy while they fit 64
bits; exact DECIMAL beyond that needs 128-bit or wider arithmetic with
MySQL's rounding, written again for the device. Text is harder: grouping,
ordering and comparison follow a collation (accent- and case-insensitive
by default, with padding rules), over variable-length values. Dictionary
indexes make low-cardinality text cheap anywhere, but joins, `ORDER BY` on
text and high-cardinality grouping need the collation's weights on the
device, or the work comes back to the host. Date and time functions in a
session time zone, and NULL handling in every aggregate, are a second
implementation of rules that are already the main source of correctness
work on the host.

**Project constraints.** The engine is written from scratch with `unsafe`
forbidden. Driving a device means a vendor runtime or a portable compute
API: a large external dependency, foreign-function calls that are `unsafe`
by nature, kernels in a second language, and a build that differs per
vendor. That is a different project policy, not an optimisation.

**Deployment.** The server ships as one container that mirrors a MySQL
database, usually beside it on a small machine. Those machines have no
such device. A GPU path would be a second execution engine that most
installations never run and every release must still test against the
same oracle, on hardware the gate does not have.

## Where it could pay

A device wins when the work per query is large against the fixed costs
(transfer or residency, launch, result copy: a few milliseconds) and is
arithmetic the device does well:

- tables of billions of rows where a scan-and-aggregate takes seconds on
  all cores, with the hot columns resident on the device;
- wide numeric aggregation or large hash joins on integer keys, where
  memory bandwidth and not decoding is the limit;
- many concurrent heavy queries on one machine that has run out of cores.

Below roughly a few hundred million rows per query, or wherever the time
is in text, DECIMAL beyond 64 bits, or small selective reads, the fixed
costs and the host round trips eat the gain. At the benchmark's size a
perfect device kernel with free transfer could remove at most the 25 ms
the query takes; the bus alone gives a quarter to all of that back.

## What is cheaper first

The same statement still spends its time in places a CPU can win back
without a device: unpacking and folding with wider vector instructions,
folding dictionary indexes at their stored width instead of widening them
to 32 bits, keeping decoded blocks out of the allocator, and answering
from per-segment aggregates when no row needs reading. Each of those is
exact by construction, needs no new dependency and helps every
installation.

## Recommendation

Do not build a GPU path. Revisit only if a deployment appears with tables
in the billions of rows, numeric-heavy queries that take seconds after the
CPU work above is done, and hardware that has a device; and then start
with a resident-column prototype for integer and 64-bit decimal
aggregation only, measured against the CPU path on that data before
anything else is written.
