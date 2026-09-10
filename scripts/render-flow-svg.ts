#!/usr/bin/env bun
// Renders docs/assets/pintail-flow.svg, the README's animated diagram of how
// Pintail replicates MySQL and answers queries.
//
// GitHub strips scripts from READMEs but plays an SVG's own animation when it
// is shown as an image, so everything here is SMIL: the one-time snapshot
// plays once when the image loads, hands over to CDC at its GTID and
// disconnects, and the steady state then loops forever. Every loop interval
// divides the loop length exactly, so repeats never drift.
//
//   bun run scripts/render-flow-svg.ts

import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'

const OUT = resolve(import.meta.dir, '..', 'docs/assets/pintail-flow.svg')
const INTRO = 4.2 // seconds of the one-time snapshot and handoff
const LOOP = 13 // seconds of the repeating steady state

const C = {
  bg: '#0b0d10', panel: '#101318', line: '#2a2f37', block: '#f5821f', hi: '#ffa552', deep: '#b85a10',
  ink: '#1d1005', soft: '#ffb070', text: '#eee8e1', muted: '#8d9098', packet: '#ffd7ad', query: '#f3f4f6',
  none: 'rgba(245,130,31,0)', glow: 'rgba(245,130,31,0.07)', hot: 'rgba(245,130,31,0.24)', frame: 'rgba(245,130,31,0.035)',
}
const n = (x: number) => String(+x.toFixed(4))
const esc = (s: string) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')

// ---------- icons (20x20 line glyphs) ----------
const ICON: Record<string, string> = {
  database: '<ellipse cx="10" cy="4" rx="7" ry="2.6"/><path d="M3 4v11c0 1.5 3.1 2.6 7 2.6s7-1.1 7-2.6V4M3 9.5c0 1.5 3.1 2.6 7 2.6s7-1.1 7-2.6"/>',
  copy: '<rect x="2" y="5" width="11" height="12" rx="2"/><path d="M7 5V3h11v12h-5"/>',
  stream: '<path d="M2 6c2.5-2 4.5 2 7 0s4.5 2 7 0M2 11c2.5-2 4.5 2 7 0s4.5 2 7 0M2 16c2.5-2 4.5 2 7 0s4.5 2 7 0"/>',
  log: '<path d="M4 4h12M4 8h12M4 12h12M4 16h7"/>',
  memory: '<rect x="4" y="4" width="12" height="12" rx="2"/><path d="M7 1v3M13 1v3M7 16v3M13 16v3M1 7h3M1 13h3M16 7h3M16 13h3"/>',
  merge: '<path d="M3 3l7 7 7-7M10 10v8M6 15l4 4 4-4"/>',
  bolt: '<path d="M11 1L3 11h6l-1 8 8-10h-6z"/>',
  api: '<path d="M7 3C4 3 5 9 2 10c3 1 2 7 5 7M13 3c3 0 2 6 5 7-3 1-2 7-5 7"/>',
  control: '<path d="M3 5h14M3 10h14M3 15h14"/><circle cx="7" cy="5" r="1.8"/><circle cx="13" cy="10" r="1.8"/><circle cx="9" cy="15" r="1.8"/>',
  terminal: '<rect x="1" y="3" width="18" height="14" rx="2"/><path d="M5 8l3 2-3 2M10 13h4"/>',
  chart: '<path d="M3 17V9M8 17V4M13 17v-6M18 17V7M1 17.5h18"/>',
  app: '<rect x="1" y="3" width="18" height="14" rx="2"/><path d="M1 7h18"/>',
  browser: '<rect x="1" y="3" width="18" height="14" rx="2"/><path d="M1 7h18M4 5h.01M6.5 5h.01"/>',
  bucket: '<path d="M2 5h16l-2 12H4zM2 5c0-1.5 3.6-2.5 8-2.5s8 1 8 2.5"/>',
}

// ---------- timing helpers ----------
const loopAnim = (inner: string) => `begin="${n(INTRO)}s" dur="${LOOP}s" repeatCount="indefinite"${inner}`

/// A fill flash on a block each time something arrives, as one looping animation.
function flashes(arrivals: number[], base: string, hot = C.hot, hold = 0.04, fade = 0.3): string {
  const points: [number, string][] = [[0, base]]
  for (const t of [...arrivals].map((a) => ((a % LOOP) + LOOP) % LOOP).sort((a, b) => a - b)) {
    const last = points[points.length - 1][0]
    if (t <= last + 0.001 || t + hold + fade >= LOOP) continue
    points.push([t, base], [t + hold, hot], [t + hold + fade, base])
  }
  points.push([LOOP, base])
  return `<animate attributeName="fill" ${loopAnim('')} calcMode="linear" keyTimes="${points.map(([t]) => n(t / LOOP)).join(';')}" values="${points.map(([, v]) => v).join(';')}"/>`
}

/// A light streak along a wire: a dash as long as the tail, slid along the
/// path. Loop comets repeat every LOOP; one-shot comets run once.
function comet(d: string, kind: string, begin: number, travel: number, opts: { reverse?: boolean; once?: boolean; seg?: number } = {}): string {
  const seg = opts.seg ?? 34
  const slide = (travel * (100 + seg)) / 100
  // Parked a little past either end: a dash parked exactly at an end still
  // draws its round cap as a dot.
  const [from, to] = opts.reverse ? [-104, seg + 4] : [seg + 4, -104]
  const anim = opts.once
    ? `begin="${n(begin)}s" dur="${n(slide)}s" fill="freeze" values="${from};${to}"`
    : `begin="${n(begin)}s" dur="${LOOP}s" repeatCount="indefinite" keyTimes="0;${n(slide / LOOP)};1" values="${from};${to};${to}"`
  return `<path class="c ${kind}" d="${d}" pathLength="100" stroke-dasharray="${seg} ${100 + seg}" stroke-dashoffset="${from}"><animate attributeName="stroke-dashoffset" calcMode="linear" ${anim}/></path>`
}

/// A small label riding along with a loop comet.
function rider(pathId: string, value: string, begin: number, travel: number, reverse = false): string {
  const a = travel / LOOP
  const keyPoints = reverse ? '1;0;0' : '0;1;1'
  return `<text class="rider" opacity="0" dy="-9">${esc(value)}` +
    `<animateMotion begin="${n(begin)}s" dur="${LOOP}s" repeatCount="indefinite" calcMode="linear" keyPoints="${keyPoints}" keyTimes="0;${n(a)};1"><mpath xlink:href="#${pathId}" href="#${pathId}"/></animateMotion>` +
    `<animate attributeName="opacity" begin="${n(begin)}s" dur="${LOOP}s" repeatCount="indefinite" calcMode="discrete" keyTimes="0;${n(a)}" values="1;0"/></text>`
}

// ---------- layout ----------
type Block = { x: number; y: number; w: number; h: number; role: 'src' | 'core' | 'sub' | 'ext' | 'group' | 'inner' | 'frame' | 'srcline'; title: string; sub?: string; icon?: string; titleY?: number; fill?: string; extra?: string; subId?: string }
const FILLED = new Set(['src', 'core', 'sub', 'ext'])

function block(b: Block): string {
  const rx = b.role === 'frame' ? 18 : 8
  const parts: string[] = []
  parts.push(`<rect class="b ${b.role}" x="${b.x}" y="${b.y}" width="${b.w}" height="${b.h}" rx="${rx}">${b.fill ?? ''}</rect>`)
  if (!FILLED.has(b.role)) {
    const ty = b.titleY ?? b.y + 26
    parts.push(`<text class="t ${b.role}" x="${b.x + 16}" y="${ty}">${esc(b.title)}</text>`)
    if (b.sub) parts.push(`<text class="s" x="${b.x + 16}" y="${ty + 18}">${esc(b.sub)}</text>`)
  } else {
    const small = b.h <= 52
    const tx = b.icon ? b.x + (small ? 40 : 46) : b.x + 16
    if (b.icon) {
      const s = small ? 0.8 : 1
      parts.push(`<g class="i ${b.role === 'src' ? 'src' : ''}" transform="translate(${b.x + 14},${n(b.y + b.h / 2 - 10 * s)}) scale(${s})">${ICON[b.icon]}</g>`)
    }
    const ty = b.sub ? b.y + b.h / 2 - (small ? 1 : 3) : b.y + b.h / 2 + 5
    parts.push(`<text class="t ${b.role === 'src' ? 'src' : ''}${small ? ' sm' : ''}" x="${tx}" y="${n(ty)}">${esc(b.title)}</text>`)
    if (b.sub) parts.push(`<text class="s" x="${tx}" y="${n(ty + (small ? 15 : 18))}">${esc(b.sub)}</text>`)
    if (b.extra) parts.push(b.extra.replaceAll('{tx}', String(tx)).replaceAll('{sy}', n(ty + (small ? 15 : 18))))
  }
  return parts.join('')
}

const P = {
  snap: 'M240 226 C272 226 270 164 300 164',
  snapSeg: 'M470 164 C490 164 490 318 516 330',
  binlog: 'M240 285 L300 285',
  cdcWal: 'M470 272 C490 272 495 185 516 185',
  walMem: 'M595 210 L595 222',
  flush: 'M650 272 L650 316',
  segEngine: 'M674 370 C700 370 700 305 720 305',
  memEngine: 'M674 247 C698 247 700 270 720 270',
  client: [161, 253, 345].map((cy) => `M890 285 C925 285 930 ${cy} 960 ${cy}`),
  engineHttp: 'M805 330 L805 400',
  httpDash: 'M890 437 C925 437 930 461 960 461',
  backup: 'M300 507 L240 510',
}

// ---------- the steady-state schedule (loop time, seconds) ----------
const binlogEmits = Array.from({ length: 20 }, (_, k) => k * 0.65)
const binlogLabels = ['INSERT', 'UPDATE', 'DELETE', 'INSERT', 'UPDATE']
const queryEmits = [0, 1, 2].map((i) => [0, 1, 2, 3].map((j) => 1.5 + i + j * 3.25))
const dashEmits = [6.0, 9.25]
const backupEmits = [9.6, 10.1, 10.6]
const pulses = [0, 2, 4, 6, 8, 10, 12]

const svg: string[] = []
svg.push(`<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 1200 620" width="1200" height="620" role="img" aria-label="Pintail replicates MySQL: a one-time snapshot, then change data capture from the binlog into columnar storage, answered by one query engine over the MySQL wire protocol and an HTTP API">`)
svg.push(`<style>
  text { font-family: "Archivo", "Helvetica Neue", Helvetica, Arial, sans-serif; }
  .s, .wl, .nl, .rider, .chip text, .tok text, .lg { font-family: "JetBrains Mono", "SFMono-Regular", Menlo, Consolas, monospace; }
  .b { fill: ${C.none}; stroke: ${C.block}; stroke-width: 1.5; }
  .b.src { stroke: ${C.block}; }
  .b.srcline { stroke: ${C.block}; stroke-width: 1.2; stroke-dasharray: 2 4; }
  .b.group { fill: ${C.glow}; stroke-width: 1.2; }
  .b.inner { stroke: ${C.deep}; stroke-width: 1.2; }
  .b.frame { fill: ${C.frame}; stroke: ${C.deep}; stroke-dasharray: 6 6; }
  .t { font-size: 16px; font-weight: 600; fill: ${C.hi}; }
  .t.sm, .t.inner, .t.srcline { font-size: 14px; }
  .t.group, .t.inner { fill: ${C.soft}; }
  .t.frame { font-size: 18px; fill: ${C.block}; }
  .s { font-size: 11px; fill: ${C.muted}; }
  .i { fill: none; stroke: ${C.hi}; stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }
  .w { fill: none; stroke: ${C.line}; stroke-width: 1.5; }
  .w.soft { stroke-dasharray: 3 6; opacity: 0.75; }
  .wl { font-size: 11px; fill: ${C.muted}; }
  .nl { font-size: 11px; fill: ${C.soft}; }
  .c { fill: none; stroke-width: 3; stroke-linecap: round; }
  .c.event, .c.chip { stroke: ${C.packet}; }
  .c.read { stroke: ${C.hi}; }
  .c.query { stroke: ${C.query}; }
  .c.result { stroke: ${C.block}; }
  .rider { font-size: 10.5px; fill: ${C.text}; text-anchor: middle; }
  .seg { fill: none; stroke: ${C.hi}; stroke-width: 1.5; }
  .chip text { font-size: 11px; font-weight: 600; letter-spacing: 0.06em; text-anchor: middle; }
  .tok rect { fill: ${C.bg}; stroke: ${C.hi}; stroke-width: 1.5; }
  .tok text { font-size: 11px; font-weight: 600; fill: ${C.hi}; text-anchor: middle; }
  .lg { font-size: 11px; fill: ${C.muted}; }
</style>`)
svg.push(`<rect width="1200" height="620" rx="16" fill="${C.panel}"/>`)

// Wires, drawn first so blocks and comets sit on top. The snapshot links
// break into faint dashes once the snapshot disconnects.
const cutAt = 3.4
svg.push(`<g>`)
svg.push(`<path id="w-snap" class="w" d="${P.snap}"><set attributeName="stroke-dasharray" to="2 7" begin="${cutAt}s" fill="freeze"/><animate attributeName="opacity" to="0.45" begin="${cutAt}s" dur="0.4s" fill="freeze"/></path>`)
svg.push(`<path id="w-snapseg" class="w" d="${P.snapSeg}"><set attributeName="stroke-dasharray" to="2 7" begin="${cutAt}s" fill="freeze"/><animate attributeName="opacity" to="0.45" begin="${cutAt}s" dur="0.4s" fill="freeze"/></path>`)
for (const [id, d] of Object.entries({ binlog: P.binlog, cdcwal: P.cdcWal, walmem: P.walMem, flush: P.flush, segengine: P.segEngine, memengine: P.memEngine, enginehttp: P.engineHttp, httpdash: P.httpDash, backup: P.backup })) {
  svg.push(`<path id="w-${id}" class="w" d="${d}"/>`)
}
P.client.forEach((d, i) => svg.push(`<path id="w-client${i}" class="w" d="${d}"/>`))
svg.push(`<path class="w soft" d="M385 470 L385 330"/><path class="w soft" d="M470 492 L500 492"/>`)
svg.push(`<text class="wl" x="270" y="304" text-anchor="middle">binlog</text>`)
svg.push(`<text class="wl" x="1060" y="120" text-anchor="middle">MySQL wire protocol</text>`)
svg.push(`<text class="wl" x="395" y="405">supervises</text>`)
svg.push(`</g>`)

// Arrival times for block flashes (loop time).
const plus = (xs: number[], d: number) => xs.map((x) => x + d)
const queryArrivals = queryEmits.flat().map((u) => u + 0.8)
const hot = {
  mysql: binlogEmits,
  cdc: plus(binlogEmits, 0.8),
  wal: plus(binlogEmits, 1.4),
  mem: plus(binlogEmits, 1.7),
  engine: [...queryArrivals, ...plus(dashEmits, 1.05)],
  http: [...plus(dashEmits, 0.6), ...plus(dashEmits, 1.3)],
  clients: queryEmits.map((xs) => plus(xs, 2.1)),
  dash: plus(dashEmits, 2.35),
  s3: plus(backupEmits, 0.7),
  control: [...pulses, ...backupEmits],
}

// Blocks.
svg.push(block({ x: 270, y: 60, w: 650, h: 510, role: 'frame', title: 'Pintail', titleY: 92, sub: 'one process · read-only replica' }))
svg.push(block({ x: 40, y: 200, w: 200, h: 150, role: 'src', icon: 'database', title: 'MySQL', sub: 'source of truth', fill: flashes(hot.mysql, C.none, C.hot, 0.03, 0.2) }))
svg.push(block({ x: 40, y: 362, w: 200, h: 40, role: 'srcline', title: 'row binlog · GTID', titleY: 387 }))
svg.push(block({ x: 40, y: 478, w: 200, h: 64, role: 'ext', icon: 'bucket', title: 'S3 storage', sub: 'backup · restore', fill: flashes(hot.s3, C.none) }))
// Snapshot: counts its chunks, then reads "complete" and fades once it disconnects.
svg.push(`<g opacity="1"><animate attributeName="opacity" to="0.38" begin="3.7s" dur="0.4s" fill="freeze"/>`)
svg.push(`<rect class="b core" x="300" y="124" width="170" height="80" rx="8"><animate attributeName="fill" begin="0.3s" dur="0.3s" repeatCount="7" values="${C.none};${C.hot};${C.none}"/></rect>`)
svg.push(`<g class="i" transform="translate(314,154) scale(1)">${ICON.copy}</g>`)
svg.push(`<text class="t" x="346" y="161">Snapshot</text>`)
svg.push(`<text class="s" x="346" y="179">copying chunks<set attributeName="opacity" to="0" begin="2.9s" fill="freeze"/></text>`)
svg.push(`<text class="s" x="346" y="179" opacity="0">complete<set attributeName="opacity" to="1" begin="2.9s" fill="freeze"/></text>`)
svg.push(`<line x1="312" y1="194" x2="458" y2="194" stroke="${C.line}" stroke-width="3" stroke-linecap="round"/>`)
svg.push(`<line x1="312" y1="194" x2="312" y2="194" stroke="${C.hi}" stroke-width="3" stroke-linecap="round"><animate attributeName="x2" begin="0.5s" dur="2.4s" from="312" to="458" fill="freeze"/></line>`)
svg.push(`</g>`)
// CDC: idle through the snapshot, then streaming from the captured position.
svg.push(`<rect class="b core" x="300" y="240" width="170" height="90" rx="8">${flashes(hot.cdc, C.none)}</rect>`)
svg.push(`<g class="i" transform="translate(314,275) scale(1)">${ICON.stream}</g>`)
svg.push(`<text class="t" x="346" y="282">CDC</text>`)
svg.push(`<text class="s" x="346" y="300">waiting<set attributeName="opacity" to="0" begin="3.9s" fill="freeze"/></text>`)
svg.push(`<text class="s" x="346" y="300" opacity="0">from GTID …:1-4812<set attributeName="opacity" to="1" begin="3.9s" fill="freeze"/></text>`)
svg.push(block({ x: 300, y: 470, w: 170, h: 74, role: 'core', icon: 'control', title: 'Control plane', sub: 'metadata · health', fill: flashes(hot.control, C.none, C.hot, 0.04, 0.25) }))
svg.push(block({ x: 500, y: 120, w: 190, h: 410, role: 'group', title: 'Storage', titleY: 146 }))
svg.push(block({ x: 516, y: 160, w: 158, h: 50, role: 'sub', icon: 'log', title: 'WAL', sub: 'per table', fill: flashes(hot.wal, C.none) }))
svg.push(block({ x: 516, y: 222, w: 158, h: 50, role: 'sub', icon: 'memory', title: 'memtable', sub: 'newest rows', fill: flashes(hot.mem, C.none) }))
svg.push(block({ x: 516, y: 284, w: 158, h: 170, role: 'inner', title: 'PTSEG files', titleY: 304, fill: flashes([4.0, 8.0], C.none, C.hot, 0.04, 0.5) }))
// Compaction glows while it merges, late in each loop.
svg.push(block({ x: 516, y: 466, w: 158, h: 50, role: 'sub', icon: 'merge', title: 'compaction', sub: 'merge versions', fill: `<animate attributeName="fill" ${loopAnim('')} keyTimes="0;${n(10.2 / LOOP)};${n(10.4 / LOOP)};${n(11 / LOOP)};${n(11.3 / LOOP)};1" values="${C.none};${C.none};${C.hot};${C.hot};${C.none};${C.none}"/>` }))
svg.push(block({ x: 720, y: 240, w: 170, h: 90, role: 'core', icon: 'bolt', title: 'Query engine', sub: 'plan · execute', fill: flashes(hot.engine, C.none) }))
svg.push(block({ x: 720, y: 400, w: 170, h: 74, role: 'core', icon: 'api', title: 'HTTP API', sub: 'REST · dashboard', fill: flashes(hot.http, C.none) }))
const clients: [string, string][] = [['mysql CLI', 'terminal'], ['BI tool', 'chart'], ['your app', 'app']]
clients.forEach(([title, icon], i) => svg.push(block({ x: 960, y: 130 + i * 92, w: 200, h: 62, role: 'ext', icon, title, sub: 'MySQL wire', fill: flashes(hot.clients[i], C.none) })))
svg.push(block({ x: 960, y: 430, w: 200, h: 62, role: 'ext', icon: 'browser', title: 'dashboard', sub: 'HTTP · browser', fill: flashes(hot.dash, C.none) }))

// PTSEG files: three filled by the snapshot, one more per flush, folded back by compaction.
const chip = (i: number) => ({ x: 530 + (i % 2) * 68, y: 318 + Math.floor(i / 2) * 40 })
;[1.6, 2.2, 2.8].forEach((at, i) => {
  const p = chip(i)
  svg.push(`<rect class="seg" x="${p.x}" y="${p.y}" width="62" height="32" rx="4" opacity="0"><set attributeName="opacity" to="1" begin="${at}s" fill="freeze"/></rect>`)
})
;[[3, 4.0], [4, 8.0]].forEach(([i, appear]) => {
  const p = chip(i)
  svg.push(`<rect class="seg" x="${p.x}" y="${p.y}" width="62" height="32" rx="4" opacity="0">` +
    `<animate attributeName="opacity" ${loopAnim('')} calcMode="linear" keyTimes="0;${n(appear / LOOP)};${n((appear + 0.05) / LOOP)};${n(10.2 / LOOP)};${n(11 / LOOP)};${n(11.2 / LOOP)};1" values="0;0;1;1;0.3;0;0"/>` +
    `<animateTransform attributeName="transform" type="translate" ${loopAnim('')} calcMode="linear" keyTimes="0;${n(10.2 / LOOP)};${n(11 / LOOP)};${n(11.25 / LOOP)};1" values="0 0;0 0;0 -40;0 0;0 0"/></rect>`)
})

// Comets.
svg.push(`<g>`)
for (let k = 0; k < 7; k += 1) {
  svg.push(comet(P.snap, 'chip', k * 0.3, 0.6, { once: true }))
  svg.push(comet(P.snapSeg, 'chip', 0.6 + k * 0.3, 0.7, { once: true }))
}
binlogEmits.forEach((u, k) => {
  svg.push(comet(P.binlog, 'event', INTRO + u, 0.8))
  svg.push(rider('w-binlog', binlogLabels[k % binlogLabels.length], INTRO + u, 0.8))
  svg.push(comet(P.cdcWal, 'event', INTRO + u + 0.85, 0.55))
  svg.push(comet(P.walMem, 'event', INTRO + u + 1.45, 0.25, { seg: 60 }))
})
;[3.4, 7.4].forEach((u) => {
  svg.push(comet(P.flush, 'chip', INTRO + u, 0.6, { seg: 50 }))
  svg.push(rider('w-flush', 'flush', INTRO + u, 0.6))
})
queryEmits.forEach((emits, i) => emits.forEach((u) => {
  svg.push(comet(P.client[i], 'query', INTRO + u, 0.8, { reverse: true }))
  svg.push(rider(`w-client${i}`, 'SELECT', INTRO + u, 0.8, true))
  svg.push(comet(P.segEngine, 'read', INTRO + u + 0.8, 0.4))
  svg.push(comet(P.memEngine, 'read', INTRO + u + 0.85, 0.35))
  svg.push(comet(P.client[i], 'result', INTRO + u + 1.3, 0.8))
  svg.push(rider(`w-client${i}`, 'rows', INTRO + u + 1.3, 0.8))
}))
dashEmits.forEach((u) => {
  svg.push(comet(P.httpDash, 'query', INTRO + u, 0.6, { reverse: true }))
  svg.push(rider('w-httpdash', 'GET', INTRO + u, 0.6, true))
  svg.push(comet(P.engineHttp, 'query', INTRO + u + 0.6, 0.45, { reverse: true }))
  svg.push(comet(P.engineHttp, 'result', INTRO + u + 1.3, 0.45))
  svg.push(comet(P.httpDash, 'result', INTRO + u + 1.75, 0.6))
  svg.push(rider('w-httpdash', 'JSON', INTRO + u + 1.75, 0.6))
})
backupEmits.forEach((u, k) => {
  svg.push(comet(P.backup, 'chip', INTRO + u, 0.7, { seg: 60 }))
  if (k === 0) svg.push(rider('w-backup', 'backup', INTRO + u, 0.7))
})
svg.push(`</g>`)

// The break on the snapshot link, and the note that says why.
svg.push(`<g opacity="0"><animate attributeName="opacity" to="1" begin="${cutAt}s" dur="0.4s" fill="freeze"/>` +
  `<path d="M277 187 L291 201 M277 201 L291 187" stroke="${C.hi}" stroke-width="2" stroke-linecap="round"/>` +
  `<text class="nl" x="256" y="184" text-anchor="end">disconnected after snapshot</text></g>`)

// The captured GTID, carried from the finished snapshot to CDC.
svg.push(`<g class="tok" opacity="0"><rect x="-86" y="-13" width="172" height="26" rx="13"/><text y="4">GTID 3E11FA47…:1-4812</text>` +
  `<set attributeName="opacity" to="1" begin="2.9s"/><animate attributeName="opacity" from="1" to="0" begin="${INTRO}s" dur="0.6s" fill="freeze"/>` +
  `<animateMotion begin="2.9s" dur="1s" fill="freeze" calcMode="spline" keyTimes="0;1" keySplines="0.4 0 0.2 1" path="M385 164 C455 176 455 210 385 222"/></g>`)

// Mode chip: snapshot, handoff, then CDC streaming.
svg.push(`<g class="chip"><rect x="700" y="74" width="200" height="26" rx="13" fill="${C.none}" stroke="${C.block}" stroke-width="1.2"><set attributeName="fill" to="${C.block}" begin="${INTRO}s" fill="freeze"/></rect>` +
  `<text x="800" y="91" fill="${C.hi}">SNAPSHOT<set attributeName="opacity" to="0" begin="2.9s" fill="freeze"/></text>` +
  `<text x="800" y="91" fill="${C.hi}" opacity="0">HANDOFF<set attributeName="opacity" to="1" begin="2.9s" fill="freeze"/><set attributeName="opacity" to="0" begin="${INTRO}s" fill="freeze"/></text>` +
  `<text x="800" y="91" fill="${C.ink}" opacity="0">CDC STREAMING<set attributeName="opacity" to="1" begin="${INTRO}s" fill="freeze"/></text></g>`)

// Legend.
const legend: [string, string][] = [[C.packet, 'change or snapshot chunk'], [C.query, 'query'], [C.block, 'result'], [C.hi, 'read from storage']]
let lx = 40
for (const [color, label] of legend) {
  svg.push(`<line x1="${lx}" y1="596" x2="${lx + 26}" y2="596" stroke="${color}" stroke-width="3" stroke-linecap="round"/><text class="lg" x="${lx + 34}" y="600">${label}</text>`)
  lx += 34 + label.length * 7 + 36
}
svg.push(`<text class="lg" x="1160" y="600" text-anchor="end">the snapshot runs once; the steady state loops</text>`)
svg.push(`</svg>`)

mkdirSync(dirname(OUT), { recursive: true })
writeFileSync(OUT, `${svg.join('\n')}\n`)
console.log(`wrote ${OUT}`)
