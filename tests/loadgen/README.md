# Deterministic CDC soak

This Bun workload owns an isolated GTID-enabled MySQL 8.4 source, snapshots a
100,000-row seed table into Pintail, and then generates deterministic insert,
update, and delete traffic. The release run lasts 30 minutes and targets 5,500
row events per second, leaving headroom above the required 5,000 events/s gate.

```sh
cd tests/loadgen
bun install --frozen-lockfile
bun run soak
```

The gate requires:

- at least 5,000 generated row events/s;
- final source/replica count and deterministic checksum convergence;
- zero dead letters;
- no more than 60 seconds or 330,000 events of observed lag;
- last-third average RSS no more than 256 MiB above the first third, maximum
  RSS no more than 512 MiB above the initial sample, and a fitted RSS slope no
  greater than 128 MiB/hour.

The harness records every sample and gate outcome in `results.json` and
`results.md`. A shorter run validates orchestration without claiming the
release gate:

```sh
SOAK_DURATION_SECONDS=30 bun run smoke
```

Set `PINTAIL_SOAK_BINARY` to reuse an existing release binary. Otherwise the
harness builds `pintail` in release mode. The process uses an explicit 256 MiB
per-query ceiling. Containers, networks, and temporary replica data are always
removed, including after failures.
