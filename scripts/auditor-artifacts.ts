import { execFileSync } from 'node:child_process'
import { cpSync, existsSync } from 'node:fs'
import { join } from 'node:path'

/** Export only reports produced by this run, including git-ignored smoke files.
 * The caller must start from a fresh clone (no preexisting ignored outputs).
 */
export function copyFreshReports(checkout: string, output: string, smoke: boolean) {
  const suffix = smoke ? '-smoke' : ''
  const git = (...args: string[]) => execFileSync('git', ['-C', checkout, ...args], { encoding: 'utf8' }).trim()
  for (const file of [`results${suffix}.json`, `results${suffix}.md`, 'mysql-baseline.json']) {
    const path = join(checkout, 'benchmark', file)
    if (!existsSync(path)) continue
    const changed = git('diff', '--numstat', '--', `benchmark/${file}`)
    // Do not apply ignore rules: smoke reports are intentionally ignored.
    const untracked = git('ls-files', '--others', '--', `benchmark/${file}`)
    if (changed || untracked) cpSync(path, join(output, file))
  }
}
