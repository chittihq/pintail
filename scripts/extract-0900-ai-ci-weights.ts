// Generates the utf8mb4_0900_ai_ci weight table by asking MySQL for it.
//
// This collation compares by primary weights alone, and WEIGHT_STRING
// returns them: none for an ignorable character, one for most, several for
// an expansion. Unlike the two older collations it weighs every character
// above the basic plane too, so the scan covers all of Unicode.
//
// The weight of a string is the weights of its characters back to back:
// the script checks that on random strings before it writes anything, since
// a character-by-character table is only the whole truth if no pair of
// characters weighs differently together.
//
// Four shapes keep the table small:
//
// - a run of consecutive characters whose single weights are equal or rise
//   by one (most scripts above the basic plane);
// - a character weighing none or several weights, indexing a shared arena;
// - a Hangul syllable, which weighs as the jamo it is composed of;
// - a range deriving two weights from the code point itself, either by
//   UCA's rule for a character it does not tabulate or counted from the
//   start of the range.
//
// The script replays those four shapes over every character and compares
// with what MySQL said, so a table that is written reproduces the scan.
//
//   MYSQL='docker exec -i <container> mysql' OUT=<table>.rs \
//     bun run scripts/extract-0900-ai-ci-weights.ts

const mysql = (process.env.MYSQL ?? 'mysql').split(' ')
const out = process.env.OUT
if (!out) throw new Error('set OUT to the Rust table path to write')

const utf8Hex = (text: string) =>
  [...Buffer.from(text, 'utf8')].map((byte) => byte.toString(16).padStart(2, '0')).join('')

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

const weigh = async (texts: string[]): Promise<string[]> => {
  const union = texts
    .map((text, index) => `SELECT ${index} AS i, _utf8mb4 0x${utf8Hex(text)} AS ch`)
    .join(' UNION ALL ')
  const rows = await ask(
    `SELECT i, HEX(WEIGHT_STRING(ch COLLATE utf8mb4_0900_ai_ci)) FROM (${union}) t ORDER BY i;`,
  )
  if (rows.length !== texts.length) throw new Error('a character MySQL would not build')
  // An ignorable character weighs nothing, which a batch prints as no field.
  return rows.map((row) => row[1] ?? '')
}

const LAST = 0x10ffff
const isSurrogate = (point: number) => point >= 0xd800 && point <= 0xdfff
const weights: Array<number[] | undefined> = new Array(LAST + 1)
const sequence = (hex: string) => {
  const parts: number[] = []
  for (let index = 0; index < hex.length; index += 4) {
    parts.push(Number.parseInt(hex.slice(index, index + 4), 16))
  }
  return parts
}

const BATCH = 1024
for (let start = 0; start <= LAST; start += BATCH) {
  const points: number[] = []
  for (let point = start; point < Math.min(start + BATCH, LAST + 1); point += 1) {
    if (!isSurrogate(point)) points.push(point)
  }
  if (points.length === 0) continue
  const answers = await weigh(points.map((point) => String.fromCodePoint(point)))
  points.forEach((point, index) => {
    weights[point] = sequence(answers[index])
  })
}
const of = (point: number) => {
  const found = weights[point]
  if (found === undefined) throw new Error(`no weight for U+${point.toString(16)}`)
  return found
}

// A string weighs what its characters weigh, one after another.
let seed = 0x9e3779b9
const random = (below: number) => {
  seed ^= seed << 13
  seed ^= seed >>> 17
  seed ^= seed << 5
  return (seed >>> 0) % below
}
const tabulated: number[] = []
for (let point = 0; point <= 0x1ffff; point += 1) {
  if (!isSurrogate(point) && !(of(point).length === 2 && of(point)[0] >= 0xfb00)) tabulated.push(point)
}
const strings: string[] = []
for (let index = 0; index < 20000; index += 1) {
  const length = 2 + random(5)
  let text = ''
  for (let at = 0; at < length; at += 1) text += String.fromCodePoint(tabulated[random(tabulated.length)])
  strings.push(text)
}
// Neighbours in the code space are the likeliest to weigh together.
for (let point = 0; point < 0x3100; point += 1) {
  if (isSurrogate(point) || isSurrogate(point + 1)) continue
  strings.push(String.fromCodePoint(point, point + 1), String.fromCodePoint(point + 1, point))
}
for (let start = 0; start < strings.length; start += BATCH) {
  const chunk = strings.slice(start, start + BATCH)
  const answers = await weigh(chunk)
  chunk.forEach((text, index) => {
    const expected = [...text].flatMap((character) => of(character.codePointAt(0)!))
    const actual = sequence(answers[index])
    if (expected.join() !== actual.join()) {
      throw new Error(`U+${[...text].map((c) => c.codePointAt(0)!.toString(16)).join(' U+')} weighs together`)
    }
  })
}

const HANGUL_FIRST = 0xac00
const HANGUL_LAST = 0xd7a3
const jamo = (point: number) => {
  const index = point - HANGUL_FIRST
  const trailing = index % 28
  return [0x1100 + Math.floor(index / 588), 0x1161 + Math.floor((index % 588) / 28)].concat(
    trailing === 0 ? [] : [0x11a7 + trailing],
  )
}
const UNASSIGNED_BASE = 0xfbc0
const derived = (point: number, parts: number[]) =>
  parts.length === 2 && parts[1] === ((point & 0x7fff) | 0x8000) ? parts[0] - (point >> 15) : undefined

type Run = [first: number, length: number, weight: number, step: number]
const runs: Run[] = []
const sequences: Array<[number, number, number]> = []
const arena: number[] = []
/// `(first, last, base, counted)`: counted ranges take their second weight
/// from the distance to `first`, the others from the code point.
const ranges: Array<[number, number, number, number]> = []

for (let point = 0; point <= LAST; point += 1) {
  if (isSurrogate(point)) continue
  const parts = of(point)
  const base = derived(point, parts)
  if (base !== undefined) {
    if (base === UNASSIGNED_BASE) continue
    const last = ranges.at(-1)
    if (last && last[3] === 0 && last[2] === base && last[1] === point - 1) last[1] = point
    else ranges.push([point, point, base, 0])
    continue
  }
  if (point >= HANGUL_FIRST && point <= HANGUL_LAST) {
    if (jamo(point).flatMap(of).join() !== parts.join()) {
      throw new Error(`Hangul U+${point.toString(16)} is not its jamo`)
    }
    continue
  }
  // Two weights counted from where a block starts.
  const counted = ranges.at(-1)
  if (
    parts.length === 2 &&
    parts[0] >= 0xfb00 &&
    counted &&
    counted[3] === 1 &&
    counted[1] === point - 1 &&
    counted[2] === parts[0] &&
    parts[1] === 0x8000 + (point - counted[0])
  ) {
    counted[1] = point
    continue
  }
  if (parts.length === 2 && parts[0] >= 0xfb00 && parts[1] === 0x8000) {
    ranges.push([point, point, parts[0], 1])
    continue
  }
  if (parts.length === 1) {
    const run = runs.at(-1)
    if (run && run[0] + run[1] === point && run[1] < 0xffff) {
      const step = parts[0] - (run[2] + run[3] * (run[1] - 1))
      if (run[1] === 1 && (step === 0 || step === 1)) {
        run[3] = step
        run[1] += 1
        continue
      }
      if (run[1] > 1 && step === run[3]) {
        run[1] += 1
        continue
      }
    }
    runs.push([point, 1, parts[0], 0])
    continue
  }
  sequences.push([point, arena.length, parts.length])
  arena.push(...parts)
}

// Replay the shapes exactly as the engine reads them.
const search = <T extends number[]>(table: T[], point: number, covers: (entry: T) => number) => {
  let low = 0
  let high = table.length
  while (low < high) {
    const middle = (low + high) >> 1
    const side = covers(table[middle])
    if (side === 0) return table[middle]
    if (side < 0) high = middle
    else low = middle + 1
  }
  return undefined
}
const replay = (point: number): number[] => {
  const run = search(runs, point, (entry) => (point < entry[0] ? -1 : point >= entry[0] + entry[1] ? 1 : 0))
  if (run) return [run[2] + run[3] * (point - run[0])]
  const several = search(sequences, point, (entry) => (point < entry[0] ? -1 : point > entry[0] ? 1 : 0))
  if (several) return arena.slice(several[1], several[1] + several[2])
  if (point >= HANGUL_FIRST && point <= HANGUL_LAST) return jamo(point).flatMap(replay)
  const range = search(ranges, point, (entry) => (point < entry[0] ? -1 : point > entry[1] ? 1 : 0))
  if (range && range[3] === 1) return [range[2], 0x8000 + (point - range[0])]
  return [(range ? range[2] : UNASSIGNED_BASE) + (point >> 15), (point & 0x7fff) | 0x8000]
}
for (let point = 0; point <= LAST; point += 1) {
  if (isSurrogate(point)) continue
  if (replay(point).join() !== of(point).join()) {
    throw new Error(`the table does not reproduce U+${point.toString(16)}`)
  }
}

const hex = (value: number, digits: number) => `0x${value.toString(16).padStart(digits, '0')}`
const rows = (entries: string[], perLine: number) => {
  const lines: string[] = []
  for (let index = 0; index < entries.length; index += perLine) {
    lines.push(`    ${entries.slice(index, index + perLine).join(', ')},`)
  }
  return lines.join('\n')
}

const source = `//! \`utf8mb4_0900_ai_ci\` weights, generated from \`MySQL\`.
//!
//! Produced by asking a real \`MySQL\` 8 server for \`WEIGHT_STRING(... COLLATE
//! utf8mb4_0900_ai_ci)\` over every character there is. Generated rather
//! than transcribed: these are the server's answers, obtained as output.
//!
//! The collation compares by these weights and nothing else, and a string
//! weighs what its characters weigh one after another - the generator
//! checks that on twenty thousand random strings and every pair of
//! neighbours before it writes the table. A character weighs none (an
//! ignorable mark), one, or several (an expansion).
//!
//! ${runs.length} runs of consecutive characters weigh one weight each, equal or
//! rising by one along the run; ${sequences.length} characters weigh none or several,
//! indexing the shared arena; a Hangul syllable weighs as its jamo; ${ranges.length}
//! ranges derive two weights from the code point; and every other
//! character follows UCA's rule for one it does not tabulate.
//!
//! Regenerate with \`scripts/extract-0900-ai-ci-weights.ts\` against any
//! \`MySQL\` 8. The tables carry \`rustfmt::skip\`: a generated table is
//! written in the shape that reads as data.

/// Runs of single weights: \`(first code point, length, first weight, step)\`.
/// The character \`n\` places into a run weighs \`first weight + step * n\`.
#[rustfmt::skip]
pub(crate) static AI_CI_RUNS: &[(u32, u16, u16, u8)] = &[
${rows(runs.map((run) => `(${hex(run[0], 4)}, ${run[1]}, ${hex(run[2], 4)}, ${run[3]})`), 5)}
];

/// Code points weighing none or several: \`(code point, arena offset, count)\`.
#[rustfmt::skip]
pub(crate) static AI_CI_SEQUENCES: &[(u32, u16, u8)] = &[
${rows(sequences.map((entry) => `(${hex(entry[0], 4)}, ${entry[1]}, ${entry[2]})`), 6)}
];

/// The weights \`AI_CI_SEQUENCES\` indexes, back to back.
#[rustfmt::skip]
pub(crate) static AI_CI_ARENA: &[u16] = &[
${rows(arena.map((weight) => hex(weight, 4)), 12)}
];

/// Ranges deriving two weights from the code point: \`(first, last, base,
/// counted)\`. A counted range weighs \`base\` then \`0x8000\` plus the
/// distance from \`first\`; the others weigh \`base + (point >> 15)\` then
/// \`(point & 0x7fff) | 0x8000\`.
#[rustfmt::skip]
pub(crate) static AI_CI_DERIVED: &[(u32, u32, u16, bool)] = &[
${rows(ranges.map((range) => `(${hex(range[0], 4)}, ${hex(range[1], 4)}, ${hex(range[2], 4)}, ${range[3] === 1})`), 3)}
];
`

if (arena.length > 0xffff) throw new Error('the arena outgrew its offsets')
console.log(`runs ${runs.length} sequences ${sequences.length} arena ${arena.length} derived ranges ${ranges.length}`)
console.log(`strings checked: ${strings.length}`)
await Bun.write(out, source)
