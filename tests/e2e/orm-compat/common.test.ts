import { describe, expect, test } from 'bun:test'
import { canonical, compareCaptured } from './common'

describe('ORM differential normalization', () => {
  test('canonicalizes driver-specific values and object key order', () => {
    expect(
      canonical({ z: 2n, a: Buffer.from([0, 255]), date: new Date('2025-01-02T03:04:05Z') }),
    ).toEqual({
      a: { binary: 'AP8=' },
      date: '2025-01-02T03:04:05.000Z',
      z: '2',
    })
  })

  test('compares normalized generated SQL as well as rows', () => {
    const results = compareCaptured(
      'sequelize',
      'lookup',
      { value: [{ id: 1n }], sql: ['Executing (default): SELECT  id FROM t'] },
      { value: [{ id: 1n }], sql: ['SELECT id FROM t'] },
    )
    expect(results).toEqual([
      { client: 'sequelize', check: 'lookup:result', status: 'PASS', detail: undefined },
      {
        client: 'sequelize',
        check: 'lookup:generated-sql',
        status: 'PASS',
        detail: undefined,
      },
    ])
  })
})
