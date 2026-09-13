import { expect, test } from 'bun:test'
import { signature } from './farm.ts'

test('the same defect on another seed shares a signature', () => {
  const first = "running 1 test\nthread 'x' panicked at crates/a.rs:12:5:\ncdc simulation seed=8 step=68"
  const second = "running 1 test\nthread 'x' panicked at crates/a.rs:12:5:\ncdc simulation seed=91 step=3"
  expect(signature(first)).toBe(signature(second))
  expect(signature(first)).toContain('panicked at crates/a.rs:N:N')
})

test('a divergence line wins over trailing noise', () => {
  const log = '[cdc-matrix] mariadb114 DIVERGED round 3, table heap: 22 source rows, 7 mirrored\n[cdc-matrix] CDC-MATRIX-FAIL: mariadb114'
  expect(signature(log)).toBe('[cdc-matrix] mariadbN DIVERGED round N, table heap: N source rows, N mirrored')
})
