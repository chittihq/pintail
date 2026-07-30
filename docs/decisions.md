# Architecture decisions

### Bun is the dashboard package manager

The goal specification originally named pnpm in the repository layout. The
owner subsequently directed Pintail to use Bun instead. Bun now owns dependency
installation, the lockfile, local scripts, CI, and container dashboard builds;
Yarn and pnpm artifacts are not accepted. This changes build tooling only and
does not change the Nuxt 4 plus shadcn-vue dashboard decision.

### Dashboard output is generated, not versioned

Nuxt's static output contains per-build identifiers and timestamps, so
committing `.output/public` creates unrelated diffs after every verification
run. `pintail-api` instead generates the dashboard when its source inputs
change and embeds that exact output. CI and the container build generate once,
then set `PINTAIL_DASHBOARD_PREBUILT=1` for the immediately following Cargo
build. The generated directory stays ignored.

### SQL parsing may use sqlparser-rs

Pintail will use `sqlparser-rs` with its MySQL dialect when the query and
replication milestones introduce SQL parsing. This is the specification's
pre-approved engine-adjacent exception: Pintail will still own its bound AST,
type semantics, planner, optimizer, and vectorized executor.

### SQLite is control-plane storage only

The embedded SQLite database stores metadata, settings, checkpoints, jobs, and
other operational state. It is never part of the analytical data path; table
data, indexes, WAL records, and execution remain Pintail-owned formats and
implementations.

### Storage data uses Pintail-owned binary formats

The M1 data path uses no serialization or database engine crate. Pintail owns
the WAL, manifest, PTSEG column layout, adaptive encodings, checksums,
merge-on-read, snapshots, and compaction rules. `xxhash-rust`, `lz4_flex`, and
`zstd` provide only the utility checksum/compression primitives permitted by
the specification. Format version 1 is documented byte-for-byte in
`docs/format.md`.

### Compaction scheduling stays outside the table core

The storage core performs at most one size-tier compaction and reclamation
step when an ingest crosses its memtable budget, reports compaction debt, and
yields between input segments. It does not create an unsupervised thread per
open table. The replication supervisor will own background scheduling when
the ingestion task tree is introduced, calling the bounded per-table
maintenance API from its managed worker. This is a deliberate staging
deviation from §5.1's final “background, per table” topology: M1 has no
executor, query scheduler, or supervisor lifetime to own and stop those
threads safely.

### Codec chunks are independently checksummed blocks

PTSEG treats each target-sized, independently checksummed block as the codec
chunk. Dictionary selection and dictionary bytes therefore belong to one
block rather than being shared across every block of a physical column. This
narrows corruption and decode scope, keeps block pruning independently
seekable, and bounds dictionary memory. It is the storage format's explicit
interpretation of §5.2's “dict per chunk”; `docs/format.md` uses “block”
consistently for that unit.

### Metadata queries use a catalog-backed compatibility path

`SHOW`, `DESCRIBE`, and the initial `information_schema` tables are immutable
catalog views rather than analytical storage tables. They share the MySQL
parser and typed result model, but do not create PTSEG snapshots merely to
answer control metadata. The compatibility path is deliberately restricted
to deterministic metadata query shapes and rejects unsupported expressions;
ordinary table queries still pass through Pintail's binder, logical planner,
optimizer, physical planner, and vectorized executor.

### Uncorrelated subqueries materialize once

M2 binds table-reading scalar and `IN` subqueries as typed nested queries and
executes each once before compiling the containing physical expression.
Materialized values count against the parent query's hard memory cap. This
keeps uncorrelated semantics deterministic without introducing a hidden
nested-loop executor. Correlated subqueries remain explicit errors until a
decorrelation or parameterized-plan design can preserve both correctness and
bounded execution.

### Projected scans parallelize independent segment work

Projected scans use a Pintail-owned Rayon thread pool, a data-path CPU utility
explicitly permitted by the goal specification. Header/zone-map reads and late
column fetches run in parallel per segment; max-version and tombstone winner
resolution remains deterministic and single-owner after the parallel results
return. This preserves merge-on-read correctness, isolates scan scheduling
from Rayon's process-global pool, and avoids exposing storage internals to the
async runtime.

### Key pruning requires explicit catalog provenance

Catalog table entries carry stable source-column IDs for each physical key
component. The executor never infers that the first visible column produced
the first storage-key component: append-row IDs, reordered schemas, and source
index choices make that inference unsafe. M2 range pruning is therefore
restricted to an explicitly declared single `Int64` or `UInt64` key and an
exact or losslessly convertible integer literal. Every other predicate falls
back to a correct full physical-key range.

### Aggregate pushdown requires a proof

The v1 optimizer pushes an aggregate through a cross join only when the
discarded input is unreferenced, predicate-free, and has exact catalog
cardinality one. Pintail does not yet catalog foreign-key or uniqueness
relationships rich enough to prove broader join/aggregate rewrites safe.
Rule coverage stays narrow rather than relying on optimistic cardinality
assumptions that could change results.

### M3 logical types reuse PTSEG version-one carriers

M3 records exact source widths, decimal precision/scale, and temporal
fractional precision in Pintail schemas and schema fingerprints. PTSEG and WAL
version one continue to encode the six physical scalar carriers established in
M1: narrow signed and unsigned integers use their 64-bit carrier, `Float32`
uses the 64-bit float carrier, and decimal, date, date-time, time, and JSON use
canonical UTF-8. Probe-time range validation prevents a narrow source value
from escaping its logical width; decimal text is lossless.

This deliberately avoids an after-M2 format rewrite or an unversioned tag
reinterpretation. It deviates from the goal's proposed i128/date integer
physical representations, so the limitation is explicit: exact decimal query
arithmetic remains future work even though snapshot and storage fidelity are
lossless. A future native representation requires a new storage format
version and migration design.

### Resumes retain the first snapshot handoff position

The control plane inserts the snapshot-to-stream checkpoint once. A restarted
snapshot opens new consistent source transactions and skips durable chunks,
but it never advances that checkpoint. M4 can therefore replay every source
change since the first attempt and make mixed-attempt chunks converge.
Replacing the position on restart would silently lose changes made to a chunk
that had already completed.

Direct snapshot chunks publish immutable segments before atomically marking
their SQLite journal entry complete. A crash in that narrow interval may
publish the same version-zero keys twice on retry; mandatory merge-on-read
makes the replay invisible, and compaction later removes the redundant
version. Snapshot traffic consequently bypasses both the mutable table and
WAL without weakening recovery.

### mysql_async uses its minimal Rust feature set

The source client is the specification-approved `mysql_async` crate. Pintail
enables its `minimal-rust`, Rustls, and ring features instead of the default
feature set so the workspace keeps its declared Rust 1.85 MSRV and does not
pull a native TLS dependency. Snapshot tests exercise the resulting client
against MySQL 5.7, MySQL 8.4, and MariaDB 11.
