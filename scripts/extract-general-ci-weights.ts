// Generates the utf8mb4_general_ci weight table by asking MySQL for it.
//
// One row per character rather than one concatenated string: CHAR() returns
// NULL for a code point it will not build, and inside CONCAT that nulls the
// whole batch, hiding which character was at fault.
import mysql from 'mysql2/promise'

const db = await mysql.createConnection({
  host: process.env.H!, port: 25060, user: 'doadmin',
  password: process.env.P!, database: 'defaultdb',
  ssl: { rejectUnauthorized: false },
})

const weights = new Map<number, number>()
const BATCH = 1024
let identity = 0
let unbuildable = 0

for (let start = 0; start <= 0xffff; start += BATCH) {
  const points: number[] = []
  for (let cp = start; cp < Math.min(start + BATCH, 0x10000); cp++) {
    if (cp >= 0xd800 && cp <= 0xdfff) continue   // surrogates are not utf8mb4
    points.push(cp)
  }
  if (!points.length) continue
  // CHAR(N USING utf8mb4) builds BYTES, not a code point, so anything needing
  // multi-byte UTF-8 comes back NULL. The character is encoded here and passed
  // as a utf8mb4 hex literal instead.
  const union = points
    .map((cp) => {
      const hex = [...Buffer.from(String.fromCodePoint(cp), 'utf8')]
        .map((byte) => byte.toString(16).padStart(2, '0'))
        .join('')
      return `SELECT ${cp} AS cp, _utf8mb4 0x${hex} AS ch`
    })
    .join(' UNION ALL ')
  const [rows] = await db.query<any[]>(
    `SELECT cp, HEX(WEIGHT_STRING(ch COLLATE utf8mb4_general_ci)) AS w FROM (${union}) t`,
  )
  for (const row of rows) {
    const cp = Number(row.cp)
    if (row.w === null || row.w.length !== 4) { unbuildable++; continue }
    const weight = parseInt(row.w, 16)
    if (weight === cp) identity++
    else weights.set(cp, weight)
  }
}

const supplementary = (cp: number) => {
  const hex = [...Buffer.from(String.fromCodePoint(cp), 'utf8')]
    .map((byte) => byte.toString(16).padStart(2, '0')).join('')
  return `HEX(WEIGHT_STRING(_utf8mb4 0x${hex} COLLATE utf8mb4_general_ci))`
}
const [supp] = await db.query<any[]>(
  `SELECT ${supplementary(0x1f600)} AS emoji, ${supplementary(0x20000)} AS cjk,
          (_utf8mb4 0xf09f9880 = _utf8mb4 0xf0a08080 COLLATE utf8mb4_general_ci) AS collapse`,
)
console.log(`identity:     ${identity}`)
console.log(`exceptions:   ${weights.size}`)
console.log(`unbuildable:  ${unbuildable}`)
console.log(`supplementary: emoji=${supp[0].emoji} cjk=${supp[0].cjk} emoji==cjk? ${supp[0].collapse}`)
await db.end()
await Bun.write(process.env.OUT!, JSON.stringify([...weights].sort((a, b) => a[0] - b[0])))
