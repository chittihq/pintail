# Pintail session-resource results

Measured 2026-09-05T04:37:51.352Z on darwin/arm64.

Idle connections: 1000. Preparing sessions: 50 × 1024 statements.
Every session authenticates against a local database; no query runs.

| phase | detail | peak RSS MB | resting RSS MB | resting over baseline MB |
|---|---|---:|---:|---:|
| baseline | server provisioned, no sessions | 25 | 18 | 0 |
| idle | 1000 authenticated connections, no statements | 24 | 17 | -0 |
| prepared | 50 sessions × 1024 prepared statements (51200 total) | 25 | 21 | 3 |
| closed | statements closed, connections still open | 27 | 25 | 8 |
| released | every connection ended | 34 | 25 | 7 |

Per idle connection: 6.4 KB (peak while opening). Per prepared statement: 0.15 KB. At the default ceilings (1000 connections, 1024 statements each), a client that fills both holds at most 157 MB of session state before the first query runs.

Per-session and per-statement costs are the peak-over-previous-phase
differences divided by the counts; they are what the connection and
prepared-statement ceilings (`--wire-max-connections`,
`--wire-max-prepared-statements`) are sized against. Peak is the reading
while a phase was being built; resting is after it sat still for five
seconds. "released" says what the process gives back: a resting RSS well
above baseline after every session ended would be a leak.
