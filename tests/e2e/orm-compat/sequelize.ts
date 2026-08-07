import { DataTypes, Op, Sequelize, col, fn } from 'sequelize'
import {
  captureFailure,
  compareCaptured,
  type Captured,
  type MysqlEndpoint,
  type OrmCompatibilityResult,
} from './common'

function client(endpoint: MysqlEndpoint, sql: string[]) {
  return new Sequelize(endpoint.database, endpoint.user, endpoint.password, {
    dialect: 'mysql',
    host: endpoint.host,
    port: endpoint.port,
    logging: (statement) => sql.push(statement),
    dialectOptions: {
      supportBigNumbers: true,
      bigNumberStrings: true,
      dateStrings: true,
    },
    pool: { max: 1, min: 0, idle: 1_000 },
  })
}

function models(sequelize: Sequelize) {
  const Customer = sequelize.define(
    'Customer',
    {
      id: { type: DataTypes.INTEGER.UNSIGNED, primaryKey: true, autoIncrement: true },
      name: { type: DataTypes.STRING(64), allowNull: false },
      email: { type: DataTypes.STRING(96), allowNull: true },
      tier: { type: DataTypes.ENUM('free', 'pro', 'enterprise'), allowNull: false },
      balance: { type: DataTypes.DECIMAL(12, 2), allowNull: false },
    },
    { tableName: 'customers', timestamps: false },
  )
  const Order = sequelize.define(
    'Order',
    {
      id: { type: DataTypes.BIGINT.UNSIGNED, primaryKey: true, autoIncrement: true },
      customer_id: { type: DataTypes.INTEGER.UNSIGNED, allowNull: false },
      status: {
        type: DataTypes.ENUM('pending', 'processing', 'shipped', 'delivered', 'cancelled'),
        allowNull: false,
      },
      total: { type: DataTypes.DECIMAL(12, 2), allowNull: false },
      placed_on: { type: DataTypes.DATEONLY, allowNull: false },
    },
    { tableName: 'orders', timestamps: false },
  )
  Customer.hasMany(Order, { as: 'orders', foreignKey: 'customer_id' })
  Order.belongsTo(Customer, { as: 'customer', foreignKey: 'customer_id' })
  return { Customer, Order }
}

async function withClient<T>(
  endpoint: MysqlEndpoint,
  run: (sequelize: Sequelize) => Promise<T>,
): Promise<Captured<T>> {
  const statements: string[] = []
  const sequelize = client(endpoint, statements)
  try {
    await sequelize.authenticate()
    statements.length = 0
    const value = await run(sequelize)
    return { value, sql: statements }
  } finally {
    await sequelize.close()
  }
}

async function parity<T>(
  check: string,
  mysql: MysqlEndpoint,
  pintail: MysqlEndpoint,
  run: (sequelize: Sequelize) => Promise<T>,
): Promise<OrmCompatibilityResult[]> {
  return captureFailure('sequelize', check, async () => {
    const expected = await withClient(mysql, run)
    const actual = await withClient(pintail, run)
    return compareCaptured('sequelize', check, expected, actual)
  })
}

export async function runSequelizeCompatibility(
  mysql: MysqlEndpoint,
  pintail: MysqlEndpoint,
): Promise<OrmCompatibilityResult[]> {
  const results: OrmCompatibilityResult[] = []
  results.push(
    ...(await parity('metadata', mysql, pintail, async (sequelize) => {
      const queryInterface = sequelize.getQueryInterface()
      const tables = (await queryInterface.showAllTables()).map(String).sort()
      const columns = await queryInterface.describeTable('customers')
      const indexes = (await queryInterface.showIndex('customers')) as Array<{
        name: string
        primary: boolean
        unique: boolean
        fields: Array<{ attribute: string }>
      }>
      return {
        tables,
        columns: Object.fromEntries(
          Object.entries(columns).map(([name, column]) => [
            name,
            {
              type: column.type,
              allowNull: column.allowNull,
              defaultValue: column.defaultValue,
              primaryKey: column.primaryKey,
              autoIncrement: column.autoIncrement,
            },
          ]),
        ),
        indexes: indexes.map((index) => ({
          name: index.name,
          primary: index.primary,
          unique: index.unique,
          fields: index.fields.map((field) => field.attribute),
        })),
      }
    })),
  )
  results.push(
    ...(await parity('point-and-filtered-reads', mysql, pintail, async (sequelize) => {
      const { Customer } = models(sequelize)
      const point = await Customer.findByPk(7, {
        attributes: ['id', 'name', 'email', 'tier', 'balance'],
        raw: true,
      })
      const filtered = await Customer.findAll({
        attributes: ['id', 'name', 'balance'],
        where: {
          [Op.and]: [{ balance: { [Op.gte]: 0 } }, { email: { [Op.not]: null } }],
        },
        order: [['id', 'ASC']],
        limit: 5,
        offset: 1,
        raw: true,
      })
      return { point, filtered }
    })),
  )
  results.push(
    ...(await parity('relation-read', mysql, pintail, async (sequelize) => {
      const { Customer, Order } = models(sequelize)
      return Customer.findAll({
        attributes: ['id', 'name'],
        include: [{ model: Order, as: 'orders', attributes: ['id', 'total'], required: false }],
        where: { id: { [Op.lte]: 3 } },
        order: [
          ['id', 'ASC'],
          [{ model: Order, as: 'orders' }, 'id', 'ASC'],
        ],
        raw: true,
      })
    })),
  )
  results.push(
    ...(await parity('grouped-aggregate', mysql, pintail, async (sequelize) => {
      const { Order } = models(sequelize)
      return Order.findAll({
        attributes: [
          'customer_id',
          [fn('COUNT', col('id')), 'order_count'],
          [fn('SUM', col('total')), 'order_total'],
        ],
        group: ['customer_id'],
        having: Sequelize.where(fn('COUNT', col('id')), { [Op.gte]: 2 }),
        order: [['customer_id', 'ASC']],
        limit: 10,
        raw: true,
      })
    })),
  )
  return results
}
