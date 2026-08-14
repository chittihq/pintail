import { expect, test } from 'bun:test'
import { pintailSpecificMinimumRegressions, type ComparableReport } from './evidence'

const report = (
  pintail: [number, number],
  control: [number, number],
  engine = 'same-engine',
  host = 'same-host',
): ComparableReport => ({
  methodology: { hostFingerprint: host, engineFingerprint: engine },
  queries: ['Q1', 'Q3'].map((name, index) => ({
    name,
    timings: {
      pintail: { minMs: pintail[index] },
      clickhouseFinal: { minMs: control[index] },
    },
  })),
})

test('flags a Pintail-only minimum regression like the withdrawn 2026-08-11 run', () => {
  const regressions = pintailSpecificMinimumRegressions(
    report([8, 8], [12, 63]),
    report([16, 21], [13, 65]),
  )
  expect(regressions).toHaveLength(2)
})

test('does not compare different engines, hosts, or general host slowdowns', () => {
  expect(
    pintailSpecificMinimumRegressions(report([8, 8], [12, 63]), report([16, 21], [24, 126])),
  ).toEqual([])
  expect(
    pintailSpecificMinimumRegressions(
      report([8, 8], [12, 63]),
      report([16, 21], [13, 65], 'new-engine'),
    ),
  ).toEqual([])
})
