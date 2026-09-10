// Generates the utf8mb4_unicode_ci weight table by asking MySQL for it.
//
// Unlike general_ci, this collation weighs a character as a SEQUENCE: 'ß'
// weighs as two 's' weights, an ignorable combining mark weighs as none, and
// a BMP CJK ideograph weighs as the two-weight implicit form. So a row here
// is a code point and the whole hex weight string MySQL returns for it, not
// a single number.
//
// One row per character rather than one concatenated string: a code point
// MySQL will not build comes back NULL, and inside CONCAT that nulls the
// whole batch, hiding which character was at fault.
//
// Talks to a `mysql` command rather than a driver so it can be pointed at a
// container or a server without a client library:
//
//   MYSQL='docker exec -i <container> mysql' OUT=<table>.rs \
//     bun run scripts/extract-unicode-ci-weights.ts

const mysql = (process.env.MYSQL ?? 'mysql').split(' ')
const out = process.env.OUT
if (!out) throw new Error('set OUT to the Rust table path to write')

const utf8Hex = (codePoint: number) =>
  [...Buffer.from(String.fromCodePoint(codePoint), 'utf8')]
    .map((byte) => byte.toString(16).padStart(2, '0'))
    .join('')

async function ask(sql: string): Promise<string[][]> {
  const child = Bun.spawn([...mysql, '-N', '--batch'], {
    stdin: new TextEncoder().encode(sql),
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [text, error, code] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (code !== 0) throw new Error(`mysql exited ${code}: ${error}`)
  return text
    .split('\n')
    .filter((line) => line.length > 0)
    .map((line) => line.split('\t'))
}

const weights = new Map<number, string>()
const BATCH = 512
let unbuildable = 0

for (let start = 0; start <= 0xffff; start += BATCH) {
  const points: number[] = []
  for (let point = start; point < Math.min(start + BATCH, 0x10000); point += 1) {
    // Surrogates are not utf8mb4 characters and cannot be built.
    if (point >= 0xd800 && point <= 0xdfff) continue
    points.push(point)
  }
  if (points.length === 0) continue
  // CHAR(N USING utf8mb4) builds BYTES, not a code point, so anything needing
  // multi-byte UTF-8 comes back NULL. The character is encoded here and passed
  // as a utf8mb4 hex literal instead.
  const union = points
    .map((point) => `SELECT ${point} AS cp, _utf8mb4 0x${utf8Hex(point)} AS ch`)
    .join(' UNION ALL ')
  const rows = await ask(
    `SELECT cp, IFNULL(HEX(WEIGHT_STRING(ch COLLATE utf8mb4_unicode_ci)), 'NULL') AS w
     FROM (${union}) t;`,
  )
  for (const [point, weight] of rows) {
    if (weight === 'NULL') {
      unbuildable += 1
      continue
    }
    weights.set(Number(point), weight)
  }
}

// Every character above the BMP weighs 0xFFFD, so all of them compare equal
// to each other. Asserted rather than tabulated: it is one fact, and the
// table would otherwise need a million rows to say it.
const [[emoji, cjk, collapse]] = await ask(
  `SELECT HEX(WEIGHT_STRING(_utf8mb4 0x${utf8Hex(0x1f600)} COLLATE utf8mb4_unicode_ci)),
          HEX(WEIGHT_STRING(_utf8mb4 0x${utf8Hex(0x20000)} COLLATE utf8mb4_unicode_ci)),
          (_utf8mb4 0x${utf8Hex(0x1f600)} = _utf8mb4 0x${utf8Hex(0x20000)} COLLATE utf8mb4_unicode_ci);`,
)

const lengths = new Map<number, number>()
for (const weight of weights.values()) {
  const count = weight.length / 4
  lengths.set(count, (lengths.get(count) ?? 0) + 1)
}

const sequence = (hex: string) => {
  const parts: number[] = []
  for (let index = 0; index < hex.length; index += 4) {
    parts.push(Number.parseInt(hex.slice(index, index + 4), 16))
  }
  return parts
}

// UCA derives a weight for a character it does not tabulate, from the code
// point itself: a base that says which block it belongs to, then the low
// fifteen bits with the top bit set. Whole scripts weigh this way - every
// unassigned code point, and the two CJK ideograph blocks - so they are kept
// as the rule they follow rather than as fifty thousand rows that restate it.
const implicitBase = (point: number, parts: number[]) =>
  parts.length === 2 && parts[1] === ((point & 0x7fff) | 0x8000)
    ? parts[0] - (point >> 15)
    : undefined

const singles: Array<[number, number]> = []
const sequences: Array<[number, number, number]> = []
const arena: number[] = []
const implicit: Array<[number, number, number]> = []

for (let point = 0; point <= 0xffff; point += 1) {
  if (point >= 0xd800 && point <= 0xdfff) continue
  const hex = weights.get(point)
  if (hex === undefined) throw new Error(`no weight for U+${point.toString(16)}`)
  const parts = sequence(hex)
  const base = implicitBase(point, parts)
  if (base !== undefined) {
    const last = implicit.at(-1)
    if (last && last[2] === base && last[1] === point - 1) last[1] = point
    else implicit.push([point, point, base])
    continue
  }
  if (parts.length === 1) {
    singles.push([point, parts[0]])
    continue
  }
  sequences.push([point, arena.length, parts.length])
  arena.push(...parts)
}

const hex4 = (value: number) => `0x${value.toString(16).padStart(4, '0')}`
const rows = <T extends number[]>(entries: T[], perLine: number) => {
  const lines: string[] = []
  for (let index = 0; index < entries.length; index += perLine) {
    lines.push(
      `    ${entries
        .slice(index, index + perLine)
        .map((entry) => `(${entry.map(hex4).join(', ')})`)
        .join(', ')},`,
    )
  }
  return lines.join('\n')
}

const source = `//! \`utf8mb4_unicode_ci\` weights, generated from \`MySQL\`.
//!
//! Produced by asking a real \`MySQL\` 8 server for \`WEIGHT_STRING(... COLLATE
//! utf8mb4_unicode_ci)\` over every buildable code point in the BMP. Generated
//! rather than transcribed: \`MySQL\`'s own table lives in GPL-licensed source,
//! and these are the same facts obtained as output.
//!
//! A character weighs a SEQUENCE here, not a number: most weigh one weight,
//! an ignorable mark weighs none, and an expansion weighs several - which is
//! why there are three tables rather than one. ${implicit.length} ranges
//! follow UCA's rule for a character it does not tabulate and are kept as
//! that rule; ${singles.length} characters weigh a single weight;
//! ${sequences.length} weigh none or several, indexing the shared arena.
//!
//! Regenerate with \`scripts/extract-unicode-ci-weights.ts\` against any
//! \`MySQL\` 8. The tables carry \`rustfmt::skip\`: a generated table is
//! written in the shape that reads as data, and rustfmt would put one
//! entry on each of seventeen thousand lines.

/// Code points weighing exactly one weight: \`(code point, weight)\`.
#[rustfmt::skip]\npub(crate) static UNICODE_CI_SINGLES: &[(u16, u16)] = &[
${rows(singles, 6)}
];

/// Code points weighing none or several: \`(code point, arena offset, count)\`.
#[rustfmt::skip]\npub(crate) static UNICODE_CI_SEQUENCES: &[(u16, u16, u16)] = &[
${rows(sequences, 5)}
];

/// The weights \`UNICODE_CI_SEQUENCES\` indexes, back to back.
#[rustfmt::skip]\npub(crate) static UNICODE_CI_ARENA: &[u16] = &[
${arena
  .reduce<number[][]>((lines, weight, index) => {
    if (index % 10 === 0) lines.push([])
    lines[lines.length - 1].push(weight)
    return lines
  }, [])
  .map((line) => `    ${line.map(hex4).join(', ')},`)
  .join('\n')}
];

/// Ranges deriving their weights from the code point: \`(first, last, base)\`.
/// A character in one weighs \`base + (point >> 15)\` then
/// \`(point & 0x7fff) | 0x8000\`.
#[rustfmt::skip]\npub(crate) static UNICODE_CI_IMPLICIT: &[(u16, u16, u16)] = &[
${rows(implicit, 4)}
];
`

console.log(`code points:  ${weights.size}`)
console.log(`unbuildable:  ${unbuildable}`)
console.log(
  `weight counts: ${[...lengths]
    .sort((left, right) => left[0] - right[0])
    .map(([count, total]) => `${count}x${total}`)
    .join(' ')}`,
)
console.log(`singles ${singles.length} sequences ${sequences.length} arena ${arena.length} implicit ranges ${implicit.length}`)
console.log(`supplementary: emoji=${emoji} cjk=${cjk} emoji==cjk? ${collapse}`)

await Bun.write(out, source)
