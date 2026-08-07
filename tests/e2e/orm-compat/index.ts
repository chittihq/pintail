import type { MysqlEndpoint, OrmCompatibilityResult } from './common'
import { runDrizzleCompatibility } from './drizzle'
import { runPrismaCompatibility } from './prisma'
import { runSequelizeCompatibility } from './sequelize'

export type { MysqlEndpoint, OrmCompatibilityResult } from './common'

export async function runOrmCompatibility(
  mysql: MysqlEndpoint,
  pintail: MysqlEndpoint,
): Promise<OrmCompatibilityResult[]> {
  return [
    ...(await runSequelizeCompatibility(mysql, pintail)),
    ...(await runDrizzleCompatibility(mysql, pintail)),
    ...(await runPrismaCompatibility(mysql, pintail)),
  ]
}
