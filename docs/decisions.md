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
