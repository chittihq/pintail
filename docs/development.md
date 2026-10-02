# Development

## Browser error reporting

The embedded dashboard reads `/api/telemetry/config` before mounting. When
`PINTAIL_SENTRY_DSN` is configured, it reports browser exceptions, unhandled
promise rejections and Nuxt errors to that project. An unset or invalid DSN
disables reporting. Configuration failures time out after two seconds and
leave the dashboard usable. No separate Nuxt server is required.

The endpoint returns only the public DSN, environment and release. A legacy
DSN's private key is stripped. Release tags use `PINTAIL_RELEASE`, then
`PINTAIL_BUILD_VERSION`, then the crate version. `PINTAIL_ENVIRONMENT` tags
the environment. Events carry `surface=dashboard`; component props, user
context, request details and breadcrumbs are excluded. Session recording,
session tracking, performance tracing, logs and metrics are disabled.

For readable stack traces, set `SENTRY_ORG`, `SENTRY_PROJECT` and
`SENTRY_AUTH_TOKEN` in the build environment before `bun run generate`.
The build uploads client source maps and removes them before embedding
the dashboard. These variables are build-only; the token is never sent
to browsers. Without all three variables, source-map generation and
upload are disabled; errors still report, with minified stack traces.

For Docker builds, pass the organization and project as build arguments
and the token as a BuildKit secret:

```sh
docker build --build-arg SENTRY_ORG --build-arg SENTRY_PROJECT \
  --secret id=sentry_auth_token,env=SENTRY_AUTH_TOKEN -t pintail:local .
```

The isolated browser regressions use synthetic APIs and a local envelope
receiver. After generating the dashboard, run `bun run dashboard` in
`tests/browser` to check result pagination, tooltip cleanup, actual browser
error delivery and reporting privacy without contacting Sentry.

## Running the dashboard against a live server

In production `pintail` serves the built dashboard itself, so the app calls the
API with relative paths — `/api/...` and `/status`. Under `nuxt dev` the app has
its own origin on port 3000, and those paths would hit Nuxt rather than the
engine. `nuxt.config.ts` therefore proxies them in development to whatever
`PINTAIL_API_URL` names, defaulting to `http://127.0.0.1:8080`.

The proxy carries `/api/events` too, which is a server-sent event stream: the
dashboard's live updates depend on it not being buffered.

### Against a server on this machine

```sh
cargo run -p pintail --bin pintail -- --data-dir .devdata --http-bind 127.0.0.1:8080
cd packages/dashboard && bun run dev
```

Nothing else to configure — the default target matches.

### Against a server in Docker

A container publishes its port on the *Docker host*. If the daemon is local,
publish 8080 and the default target already reaches it:

```sh
docker run -d --name pintail-dev -p 8080:8080 <image> \
  --data-dir /var/lib/pintail --http-bind 0.0.0.0:8080
cd packages/dashboard && bun run dev
```

If the daemon is remote — `DOCKER_HOST=ssh://<host>` or a `docker context`
over SSH — then `localhost:8080` on your machine is not the container's port.
Forward it first, then point the proxy at the local end of the tunnel:

```sh
# Publish on the remote host, then bring that port to this machine.
docker run -d --name pintail-dev -p 18080:8080 <image> \
  --data-dir /var/lib/pintail --http-bind 0.0.0.0:8080
ssh -N -L 18080:127.0.0.1:18080 <your-docker-host> &
PINTAIL_API_URL=http://127.0.0.1:18080 bun run dev
```

Two details decide whether this works, and both are easy to get wrong:

- The container must bind `0.0.0.0` **inside** the container. Binding
  `127.0.0.1` there means the published port forwards to nothing.
- `-p` must publish the port on the host. Without it the tunnel finds no
  listener, and the symptom is a connection refused rather than an error that
  names the cause.

Verified end to end against a container on a remote SSH daemon: a request to
the dev server's `/status` was answered by the container, through the tunnel
and the proxy.

Any reachable address works the same way:

```sh
PINTAIL_API_URL=http://10.0.0.5:8080 bun run dev
```

### Checking the proxy rather than guessing

With both running, these should answer from the engine, not from Nuxt:

```sh
curl -s localhost:3000/status
# {"status":"ready","version":"0.1.0",...}

curl -s localhost:3000/api/databases
# {"error":"Bearer authentication is required"}
```

That 401 is the point: it is pintail's own response. HTML back from either
means the proxy is not reaching the server — check `PINTAIL_API_URL`, and check
the tunnel is still up if you are using one.

Sign in through the dashboard as usual; the first-boot admin credentials are
printed by the server on its first start.

### Do not point this at a shared deployment

The dashboard is a control plane: it can start snapshots, change replication
modes, rotate keys and delete databases. Run it against a server you started
for yourself. In particular, never point it at the deployed compose stack —
`docs/limitations.md` describes the dashboard as a local control plane and not
a multi-tenant security boundary, and that assumption is what makes it safe.

## Which build and which paths a server is running

At startup, after the `pintail limits:` line, the server logs two more
lines of `key=value` pairs. `GET /api/storage` carries the same facts as
its `optimizations` object, for an operator without access to the log.
Every value is read from the code that decides it, not from a second copy
of the rule.

`pintail optimizations:` describes the machine and the binary:

| Key | Meaning |
|---|---|
| `cpu_model` | The processor's model name (Linux; `unknown` elsewhere). |
| `cpu_cores` | Logical cores available to the process. |
| `cpu_features` | Of `sse4.2`, `avx`, `avx2`, `fma`, `bmi2`, `avx512f`, `avx512bw`, `avx512vl`, `avx512dq` (or `neon`), the ones the processor reports. |
| `simd` | The level the vector kernels dispatch to: `baseline`, `avx2` or `avx512`. |
| `simd_setting` | `PINTAIL_SIMD` when set, else `unset`. |
| `build_version` | The package version. |
| `build_target` | The instruction-set level the binary was compiled for: `generic`, `x86-64-v2`, `x86-64-v3` or `x86-64-v4`. |
| `build_variant` | `standard`, or `pgo` / `pgo+bolt` for a profile-guided build. |
| `debug_assertions` | `on` in a development build. |

`pintail paths:` describes what runs:

| Key | Meaning | Controlled by |
|---|---|---|
| `scan_threads`, `scan_threads_from` | Width of the scan pool, and `cores` or the variable that set it. | `PINTAIL_SCAN_THREADS` |
| `execute_threads`, `execute_threads_from` | The same for the execute pool. | `RAYON_NUM_THREADS` |
| `inline_statements` | Bounded statements run on the connection's task. | `PINTAIL_INLINE_STATEMENTS=0` |
| `plan_cache` | Plans of repeated statements are kept (4,096 entries, 64 MiB by default). | `PINTAIL_PLAN_CACHE=0`, `PINTAIL_PLAN_CACHE_ENTRIES`, `PINTAIL_PLAN_CACHE_BYTES` |
| `small_reads` | Where a small read runs: `alone` (the default), `hold`, `handover` or `worker`. | `PINTAIL_SMALL_READS` |
| `shared_queries` | Identical concurrent requests share one execution. | `PINTAIL_DISABLE_SHARED_QUERIES` |
| `secondary_index` | Lookups through the side index. | `PINTAIL_SECONDARY_INDEX=0` |
| `settled_memo` | A settled aggregate replays its answer. | `PINTAIL_DISABLE_SETTLED_MEMO` |
| `packed_group` | Composite `GROUP BY` keys fold packed. | `PINTAIL_DISABLE_PACKED_GROUP` |
| `grouped_fold` | Grouped aggregates fold a segment at a time. | `PINTAIL_DISABLE_GROUPED_FOLD` |
| `argument_projection` | Computed aggregate arguments are projected once. | `PINTAIL_DISABLE_ARGUMENT_PROJECTION` |
| `fused_fold` | An aggregate over a scan folds each slice of the table on the worker that decoded it, on the execute pool; the scan pool takes no part in such a statement. | `PINTAIL_DISABLE_FUSED_FOLD` |
| `segment_format` | The segment format version this build writes. | |
| `size_overrides` | Memtable and compaction sizes set by `PINTAIL_MEMTABLE_KB`, `PINTAIL_COMPACTION_INPUT_ROWS`, `PINTAIL_COMPACTION_OUTPUT_ROWS`. | |
| `non_default` | Every setting that moves the process off its defaults: a path turned off, a pool resized, a size override, a tuning or diagnostic variable that is set (named without its value). | |

An empty `non_default=[]` is a deployment running as shipped. The block
cache's budget is on the `pintail limits:` line.

## Profiling a query

Every `EXPLAIN ANALYZE` prints, after the plan and the spill line, a profile
of the execution it ran: one line per plan node in plan order, with the
time spent inside the node (`total`, its inputs included, and `self`, its
inputs excluded), the time to its first batch, the batches and rows it
produced, and the highest query memory reservation seen after one of its
pulls.

```
Profile total=612.4ms spill_files=0 spill_bytes=0 peak_spill_handles=0
Sort keys=1 total=611.9ms self=0.3ms first=611.9ms batches=1 rows=40 peak_reserved=12.1MiB
  HashAggregate keys=2 aggregates=5 total=611.6ms self=402.0ms first=611.6ms batches=1 rows=40 peak_reserved=12.1MiB
    HashJoin kind=Left keys=1 residual=false total=209.6ms self=61.2ms first=3.1ms batches=3 rows=4412 peak_reserved=9.8MiB
      KeyFilter ...
```

For development only, `PINTAIL_PROFILE=1` makes the server profile every
query it runs and log the same block at info level, tagged with the
database and the first characters of the statement. Never set it on a
deployment: it logs a block per query, and the shipped compose file does
not forward it. A settled aggregate memoizes its answer, so a repeated
query profiles as a replay; `PINTAIL_DISABLE_SETTLED_MEMO=1` makes every
run execute.

An unprofiled execution builds no recorder and pays nothing for this.

`PINTAIL_SIMD` pins the instruction set the vector kernels run at, for
ruling them in or out of a slow or a wrong answer. It is read once, at the
first kernel call: `off` (or `portable`) runs every kernel's portable
fallback, `avx2` is the default where the CPU has AVX2, and `avx512` runs
the auto-vectorized kernels at AVX-512. The answers are the same at every
level.

### A timeline instead of a table

`PINTAIL_QUERY_TRACE_JSON=<file>` writes the same per-statement phases as a
Chrome trace. Open the file in [ui.perfetto.dev](https://ui.perfetto.dev) or
`chrome://tracing` and each statement is a span with its phases nested
inside, one row per worker thread, so statements that ran at the same time
read as concurrent. `PINTAIL_QUERY_TRACE=<file>` writes the same marks as
one tab-separated line per statement; both can be set at once. Neither
writes the statement text - a literal can be a row value - so a trace joins
to a case by hash.

The array in the JSON file is left unterminated on purpose, so a trace is
readable from a server that is still running. Both viewers accept that.

### When the problem is between tasks, not inside one

A profiler measures work that happened. It cannot show you a task parked on
a lock nobody expected it to want, a connection task that ended without
saying why, or a runtime whose workers are all busy while nothing
progresses. `tokio-console` shows those:

```
RUSTFLAGS="--cfg tokio_unstable" cargo build --features console -p pintail
# run that binary, then, beside it:
tokio-console
```

It is deliberately awkward. `--cfg tokio_unstable` changes tokio's ABI, so
the whole tree rebuilds and the result is not the binary you ship; the task
view also answers questions about the process to anything that reaches its
port. Build it on purpose, look, and throw it away. The feature is off by
default and is not enabled in any release profile or compose file.
