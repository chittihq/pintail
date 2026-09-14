import { expect, test } from 'bun:test'
import { signature } from './farm.ts'

test('the same defect on another seed shares a signature', () => {
  const first = "running 1 test\nthread 'x' panicked at crates/a.rs:12:5:\ncdc simulation seed=8 step=68"
  const second = "running 1 test\nthread 'x' panicked at crates/a.rs:12:5:\ncdc simulation seed=91 step=3"
  expect(signature(first)).toBe(signature(second))
  expect(signature(first)).toContain('panicked at crates/a.rs:N:N')
})

test('two defects at one panic site keep their own signatures', () => {
  const site = "thread 'x' panicked at crates/a.rs:12:5:"
  const lost = signature(`${site}\nrow 4 of table heap is missing`)
  const extra = signature(`${site}\nrow 9 of table heap is duplicated`)
  expect(lost).not.toBe(extra)
  expect(lost).toContain('row N of table heap is missing')
})

test('a divergence line wins over trailing noise', () => {
  const log = '[cdc-matrix] mariadb114 DIVERGED round 3, table heap: 22 source rows, 7 mirrored\n[cdc-matrix] CDC-MATRIX-FAIL: mariadb114'
  expect(signature(log)).toBe('[cdc-matrix] mariadb114 DIVERGED round N, table heap: N source rows, N mirrored')
})

test('a version in a name is part of the name, not a number', () => {
  const line = (leg: string) => `[cdc-matrix] ${leg} DIVERGED round 3, table heap: 22 source rows, 7 mirrored`
  expect(signature(line('mysql84'))).not.toBe(signature(line('mysql57')))
  expect(signature(line('mariadb114'))).not.toBe(signature(line('mariadb106')))
  expect(signature('DIVERGED on utf8mb4_0900_ai_ci')).toContain('utf8mb4_0900_ai_ci')
})

test('an address varies between runs of one defect', () => {
  const at = (address: string) => `thread 'x' panicked at a.rs:1:2:\nsegment ${address} is unreadable`
  expect(signature(at('0x7f3a1c00'))).toBe(signature(at('0x55e9b420')))
})
