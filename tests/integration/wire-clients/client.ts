import mysql from 'mysql2/promise'

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
console.log(JSON.stringify({ rows, metadata }))
await connection.end()
