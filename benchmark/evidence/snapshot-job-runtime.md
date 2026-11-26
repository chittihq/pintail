# Snapshot job runtime: one thread vs a thread per copy worker

Measured 2026-11-27 on an 8-vCPU box (AMD EPYC, kvm), release build, one
binary, runs alternated, `PINTAIL_SNAPSHOT_THREADS=1` (the single-threaded
job runtime used before) against the default of four (one per copy
worker).

Source: MySQL 8.4 in a container on the same box, six tables of 600,000
rows each (3.6 million rows; BIGINT key, INT, DECIMAL(12,2), VARCHAR(64),
DATETIME, DOUBLE). Timed from `POST /snapshot` until a COUNT over all six
tables on the mirror returns 3.6 million: the snapshot reports streaming
before its background copies finish, so the state alone is not the end.

| Job runtime threads | Wall time (3 runs)  | Median  | Process CPU |
|---------------------|---------------------|---------|-------------|
| 1                   | 4.40, 4.75, 4.66 s  | 4.66 s  | 7-8 s       |
| 4                   | 3.57, 3.67, 3.63 s  | 3.63 s  | 10-11 s     |

The copy is 22% faster in wall time and spends about a third more CPU.
Row conversion and segment encoding already ran on the ingest pool; what
the extra threads recover is the overlap between one worker's segment
write and the others' source reads, which the single thread serialised.
A source behind real network latency should gain more; a single-table
copy cannot gain, since its chunks run in sequence.
