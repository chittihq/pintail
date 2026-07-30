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

Projected scans use Rayon's dedicated worker pool, a data-path CPU utility
explicitly permitted by the goal specification. Header/zone-map reads and
late column fetches run in parallel per segment; max-version and tombstone
winner resolution remains deterministic and single-owner after the parallel
results return. This preserves merge-on-read correctness and stable output
ordering without exposing storage internals to the async runtime.

### Aggregate pushdown requires a proof

The v1 optimizer pushes an aggregate through a cross join only when the
discarded input is unreferenced, predicate-free, and has exact catalog
cardinality one. Pintail does not yet catalog foreign-key or uniqueness
relationships rich enough to prove broader join/aggregate rewrites safe.
Rule coverage stays narrow rather than relying on optimistic cardinality
assumptions that could change results.
