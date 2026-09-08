#!/usr/bin/env bun
/// Rewrites the README's benchmark section from `benchmark/results.json`.
///
/// The table was maintained by hand and had drifted: it advertised Pintail at
/// 152-220ms where the artifact recorded 8-69ms, so the published numbers were
/// from a run nobody could identify. Generating it means the README can only
/// ever say what the last committed run measured.
///
/// Two tables, never merged. Pintail answers a repeated query from its settled
/// aggregate memo while ClickHouse's query cache is off and it executes every
/// time, so that comparison measures one engine's cache against the other's
/// execution. The novel-query table is the one where both execute, and it is
/// the one that speaks to engine speed - today ClickHouse wins it.
///
///   bun run benchmark/render-readme-table.ts [--check]
///
/// `--check` exits non-zero if the README is out of date, for CI.

import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

const repository = join(import.meta.dir, '..')
const artifactPath = join(repository, 'benchmark', 'results.json')
const readmePath = join(repository, 'README.md')

const BEGIN = '<!-- benchmark:begin -->'
const END = '<!-- benchmark:end -->'

interface Row {
  name: string
  mysqlMs: number
  pintailMs: number
  clickhouseFinalMs: number
  speedupVsClickhouse: number
}

interface ConcurrencyPoint {
  clients: number
  throughput: number
  p95Ms: number
  errors: number
}

interface ConcurrencyRow {
  /// `mixed Q2–Q8` or a single query's name; older artifacts named the
  /// pinned query under `query`.
  workload?: string
  query?: string
  pintail: ConcurrencyPoint
  clickhouse: ConcurrencyPoint
}

interface Artifact {
  generatedAt: string
  rows: Record<string, number>
  queries: Row[]
  novelQueries: Row[]
  /// The same canonical queries as `queries`, run with the settled memo off:
  /// the like-for-like engine-speed comparison. Absent in artifacts banked
  /// before this field existed.
  engineQueries?: Row[]
  concurrency?: ConcurrencyRow[]
  methodology: { pintailPlacement: string; iterations: string }
}

const ms = (value: number) => `${Math.round(value).toLocaleString()} ms`

/// The mixed-workload concurrency sweep, when the artifact carries one. An
/// artifact from before the sweep was mixed (a single query pinned to every
/// client) renders nothing here rather than a table that would be misread.
function concurrencySection(artifact: Artifact): string[] {
  const rows = (artifact.concurrency ?? []).filter((row) =>
    (row.workload ?? row.query ?? '').startsWith('mixed'),
  )
  if (rows.length === 0) return []
  const workload = rows[0].workload ?? rows[0].query
  return [
    `**Concurrency — ${workload}.** Simultaneous clients, each call taking the`,
    'next of Q2 through Q8 in turn, against both engines executing (memo off,',
    'query cache off). Completed queries per second and the p95 latency, which',
    'together show whether an engine holds its latency while it adds throughput.',
    '',
    '| Clients | Pintail /s | Pintail p95 | CH /s | CH p95 |',
    '|---:|---:|---:|---:|---:|',
    ...rows.map(
      (row) =>
        `| ${row.pintail.clients} | ${row.pintail.throughput}` +
        `${row.pintail.errors ? ` (${row.pintail.errors} errors)` : ''} | ${row.pintail.p95Ms} ms | ` +
        `${row.clickhouse.throughput}${row.clickhouse.errors ? ` (${row.clickhouse.errors} errors)` : ''} | ` +
        `${row.clickhouse.p95Ms} ms |`,
    ),
    '',
  ]
}

/// Strips the `Qn: ` / `Nn: ` prefix — the ordinal is an artifact of the
/// harness and means nothing to a reader.
const label = (name: string) => name.replace(/^[QN]\d+:\s*/, '')

/// The like-for-like table: the canonical queries with the settled memo
/// off, so both engines actually execute. Shown first because it is the
/// number that answers "how fast is the engine", not "how fast is the
/// cache" — see the memo table below it for the latter.
function engineSpeedSection(artifact: Artifact): string[] {
  if (!artifact.engineQueries || artifact.engineQueries.length === 0) return []
  return [
    '**Engine speed — memo off, both engines execute.** The same eight',
    "queries against Pintail restarted with its settled aggregate memo off,",
    'on the same replica as the memo table below. This is the honest',
    'engine-speed comparison, and ClickHouse is still faster on the grouped',
    'and joined shapes.',
    '',
    '| Query | MySQL | Pintail (no memo) | CH RMT+FINAL | vs CH |',
    '|---|---:|---:|---:|---:|',
    ...artifact.engineQueries.map(
      (row) =>
        `| ${label(row.name)} | ${ms(row.mysqlMs)} | ${ms(row.pintailMs)} | ` +
        `${ms(row.clickhouseFinalMs)} | ${row.speedupVsClickhouse.toFixed(2)}× |`,
    ),
    '',
  ]
}

function render(artifact: Artifact): string {
  const orders = artifact.rows.orders?.toLocaleString() ?? 'the'
  const lines: string[] = [
    BEGIN,
    '',
    `Eight reporting queries over ${orders} rows, with MySQL, Pintail and`,
    "ClickHouse each in identical containers (8 CPUs, 8 GB). A result only counts",
    "if it exactly matches MySQL's answer. Two numbers matter here and they say",
    'different things, so they are reported separately rather than averaged into',
    'one headline.',
    '',
    ...engineSpeedSection(artifact),
    '**Repeated queries — memo hit vs execution.** Pintail keeps an exact-result',
    'memo for aggregates over a settled snapshot, invalidated by any ingest, so',
    "re-running the same query on an unchanged replica is served from it.",
    "ClickHouse's query cache is off, so this compares Pintail's cache against",
    "ClickHouse's execution — a fair measure of what a dashboard refresh costs,",
    'and not a measure of engine speed.',
    '',
    '| Query | MySQL | Pintail (memo) | CH RMT+FINAL |',
    '|---|---:|---:|---:|',
    ...artifact.queries.map(
      (row) =>
        `| ${label(row.name)} | ${ms(row.mysqlMs)} | ${ms(row.pintailMs)} | ${ms(row.clickhouseFinalMs)} |`,
    ),
    '',
    '**Novel queries — memo-cold constants.** Distinct predicate variants the',
    'memo has never seen, run once per engine with no warmup, so neither the',
    'memo nor any plan cache is warm. A second, independent read on the same',
    "engine-speed question above.",
    '',
    '| Query | MySQL | Pintail | CH RMT+FINAL | vs CH |',
    '|---|---:|---:|---:|---:|',
    ...artifact.novelQueries.map(
      (row) =>
        `| ${label(row.name)} | ${ms(row.mysqlMs)} | ${ms(row.pintailMs)} | ` +
        `${ms(row.clickhouseFinalMs)} | ${row.speedupVsClickhouse.toFixed(2)}× |`,
    ),
    '',
    ...concurrencySection(artifact),
    'ClickHouse is measured in both configurations: plain `MergeTree` for its',
    'raw-speed ceiling, and `ReplacingMergeTree` read with `final = 1`, which is',
    'the comparable one because it does the merge-on-read work a CDC replica owes',
    'on every read. Full numbers, including the MergeTree column and per-query',
    'resource use, are in [benchmark/results.md](benchmark/results.md). Reproduce',
    'them with:',
    '',
    '```sh',
    '(cd benchmark && bun install --frozen-lockfile && bun run benchmark)',
    '```',
    '',
    'Caveats worth stating plainly: one synthetic dataset and eight query shapes,',
    'on a shared host, measured as',
    `\`${artifact.methodology.iterations}\`. Enough to characterise these`,
    'queries and not enough to support a general claim about either engine. MySQL',
    'runs with a 1 GB buffer pool, so its column is a baseline being escaped',
    'rather than a tuned competitor.',
    '',
    `<sub>Generated from \`benchmark/results.json\` (${artifact.generatedAt}) by`,
    '`benchmark/render-readme-table.ts` — do not edit by hand.</sub>',
    '',
    END,
  ]
  return lines.join('\n')
}

const artifact = JSON.parse(readFileSync(artifactPath, 'utf8')) as Artifact
const readme = readFileSync(readmePath, 'utf8')
const begin = readme.indexOf(BEGIN)
const end = readme.indexOf(END)
if (begin === -1 || end === -1) {
  console.error(`README is missing the ${BEGIN} / ${END} markers`)
  process.exit(1)
}

const updated = readme.slice(0, begin) + render(artifact) + readme.slice(end + END.length)
if (process.argv.includes('--check')) {
  if (updated !== readme) {
    console.error('README benchmark table is stale — run: bun run benchmark/render-readme-table.ts')
    process.exit(1)
  }
  console.log('README benchmark table matches the artifact')
} else {
  writeFileSync(readmePath, updated)
  console.log('README benchmark table regenerated')
}
