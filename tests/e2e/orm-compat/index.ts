import type { MysqlEndpoint, OrmCompatibilityResult } from './common'
import { runSequelizeCompatibility } from './sequelize'

export type { MysqlEndpoint, OrmCompatibilityResult } from './common'

export async function runOrmCompatibility(
  mysql: MysqlEndpoint,
  pintail: MysqlEndpoint,
): Promise<OrmCompatibilityResult[]> {
  return runSequelizeCompatibility(mysql, pintail)
}
