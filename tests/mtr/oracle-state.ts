import type mysql from 'mysql2/promise'

const grantTables = [
  'user', 'db', 'tables_priv', 'columns_priv', 'procs_priv', 'proxies_priv',
  'global_grants', 'role_edges', 'default_roles', 'password_history',
] as const

async function globalVariables(connection: mysql.Connection): Promise<Map<string, string>> {
  const [rows] = await connection.query<mysql.RowDataPacket[]>('SHOW GLOBAL VARIABLES')
  return new Map(rows.map((row) => [String(row.Variable_name), String(row.Value)]))
}

/** A file may mutate its oracle, but those mutations must not poison later files. */
export async function captureOracleState(connection: mysql.Connection) {
  const globals = await globalVariables(connection)
  for (const table of grantTables) {
    // Temporary copies belong only to the supervisor connection. Fixtures cannot
    // overwrite them, and authentication data never leaves the disposable oracle.
    await connection.query(`CREATE TEMPORARY TABLE mysql.pintail_mtr_saved_${table} AS SELECT * FROM mysql.${table}`)
  }
  return {
    async restore() {
      const current = await globalVariables(connection)
      // Restore write admission before restoring the grant tables.
      const names = ['super_read_only', 'read_only', ...globals.keys()]
      for (const name of new Set(names)) {
        const value = globals.get(name)
        if (value === undefined || current.get(name) === value) continue
        if (!/^[a-zA-Z0-9_]+$/.test(name)) throw new Error(`Invalid oracle variable name: ${name}`)
        const literal = /^\d+$/.test(value) ? value : connection.escape(value)
        await connection.query(`SET GLOBAL \`${name}\` = ${literal}`)
      }
      for (const table of grantTables) {
        await connection.query(`DELETE FROM mysql.${table}`)
        await connection.query(`INSERT INTO mysql.${table} SELECT * FROM mysql.pintail_mtr_saved_${table}`)
      }
      await connection.query('FLUSH PRIVILEGES')
    },
  }
}
