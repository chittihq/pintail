import mysql from 'mysql2/promise'
import { readFileSync } from 'node:fs'

const connection = await mysql.createConnection({
  host: process.env.PINTAIL_WIRE_HOST,
  port: Number(process.env.PINTAIL_WIRE_PORT),
  user: 'analytics',
  password: 'pk_wire_secret',
  database: 'analytics',
})

const [rows] = await connection.execute(
  'SELECT id, name FROM events WHERE id = ?',
  [2],
)
const [metadata] = await connection.query(
  "SELECT table_name, column_name, ordinal_position FROM information_schema.columns WHERE table_schema = 'analytics' ORDER BY ordinal_position",
)
const metadataQueries = readFileSync(new URL('metadata.sql', import.meta.url), 'utf8')
  .split(';')
  .map((query) => query.trim())
  .filter(Boolean)
const corpus = []
for (const query of metadataQueries) {
  const [result] = await connection.query(query)
  corpus.push(result)
}
console.log(JSON.stringify({ rows, metadata, corpus }))
await connection.end()
