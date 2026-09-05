import { expect, test } from 'bun:test'
import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { copyFreshReports } from './auditor-artifacts.ts'

test('exports ignored smoke reports but never unchanged historical evidence', () => {
  const root = mkdtempSync(join(tmpdir(), 'pintail-artifact-test-'))
  try {
    const output = join(root, 'out')
    mkdirSync(output)
    mkdirSync(join(root, 'benchmark'))
    execFileSync('git', ['init', '--quiet', root])
    writeFileSync(join(root, '.gitignore'), 'benchmark/results-smoke.*\n')
    writeFileSync(join(root, 'benchmark/mysql-baseline.json'), 'historical')
    execFileSync('git', ['-C', root, 'add', '.gitignore', 'benchmark/mysql-baseline.json'])
    copyFreshReports(root, output, true)
    expect(existsSync(join(output, 'mysql-baseline.json'))).toBe(false)
    writeFileSync(join(root, 'benchmark/results-smoke.json'), 'fresh smoke')
    writeFileSync(join(root, 'benchmark/mysql-baseline.json'), 'fresh baseline')
    copyFreshReports(root, output, true)
    expect(readFileSync(join(output, 'results-smoke.json'), 'utf8')).toBe('fresh smoke')
    expect(readFileSync(join(output, 'mysql-baseline.json'), 'utf8')).toBe('fresh baseline')
  } finally { rmSync(root, { recursive: true, force: true }) }
})
