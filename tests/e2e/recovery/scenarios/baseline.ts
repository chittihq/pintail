import type { Scenario } from '../harness'
/** docs/design/recovery-suite.md §0: the comparator and later writes must work without a fault. */
export const baseline: Scenario = { slug: 'baseline', area: 'baseline', promise: 'docs/design/recovery-suite.md §0', run: async () => {} }
