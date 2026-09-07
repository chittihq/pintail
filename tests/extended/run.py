#!/usr/bin/env python3
"""Run isolated release correctness layers and preserve replayable evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]


def positive(raw):
    value = int(raw)
    if not 1 <= value <= 100_000:
        raise argparse.ArgumentTypeError('must be between 1 and 100000')
    return value


def seeds_for(commit, shard):
    # Disjoint odd seeds: the generator forces its input seed's low bit to one.
    rotating = int(hashlib.sha256(commit.encode()).hexdigest()[:12], 16)
    return [0xDEADBEEF + shard * 2,
            ((rotating << 4) | (shard << 1) | 1)]


def plan(layer, shard, cases, meta_cases, seeds, output):
    cargo = os.environ.get('CARGO', str(Path.home() / '.cargo/bin/cargo'))
    base = [cargo, 'test', '--locked', '-p', 'pintail-sqllogic', '--test', 'mysql_oracle']
    if layer == 'core':
        return [
            ('sql-regressions', [cargo, 'test', '--locked', '-p', 'pintail-sqllogic'], {}),
            ('storage-faults', [cargo, 'test', '--locked', '-p', 'pintail-failpoint',
                               '-p', 'pintail-store', '-p', 'pintail-meta', '--all-features'], {}),
            ('reuse-prototype', [cargo, 'test', '--locked', '--manifest-path',
                                 'experiments/query-reuse/Cargo.toml'], {}),
        ]
    stages = []
    if shard == 0:
        stages.append(('fixed-oracle', base + ['matches_configured_mysql_for_fixed_corpus',
                       '--', '--exact', '--ignored', '--nocapture'],
                       {'PINTAIL_ORACLE_EVIDENCE': str(output / 'oracle.json')}))
    stages.append(('generated-oracle', base + ['fuzzes_against_configured_mysql',
                   '--', '--exact', '--ignored', '--nocapture'],
                   {'PINTAIL_FUZZ_CASES': str(cases),
                    'PINTAIL_FUZZ_SEEDS': ','.join(map(str, seeds)),
                    'PINTAIL_FUZZ_CORPUS_PATH': str(output / 'generated.sql')}))
    for index, seed in enumerate(seeds):
        stages.append((f'metamorphic-{index}', base + ['metamorphic_equivalences_hold',
                       '--', '--exact', '--nocapture'],
                       {'PINTAIL_META_CASES': str(meta_cases), 'PINTAIL_META_SEED': str(seed)}))
    return stages


def passed_tests(log):
    return sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', log)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--layer', choices=['sql', 'core'], required=True)
    parser.add_argument('--shard', type=int, choices=range(4), default=0)
    parser.add_argument('--cases', type=positive, default=2500)
    parser.add_argument('--meta-cases', type=positive, default=1000)
    parser.add_argument('--mysql', choices=['8.0', '8.4'], default='8.4')
    parser.add_argument('--seed-commit', help='Replay seed derivation from a recorded commit')
    parser.add_argument('--output', type=Path, default=ROOT / 'tests/extended/output')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    seeds = seeds_for(args.seed_commit or commit, args.shard)
    stages = plan(args.layer, args.shard, args.cases, args.meta_cases, seeds, output)
    report = {'commit': commit, 'seed_commit': args.seed_commit or commit,
              'layer': args.layer, 'shard': args.shard, 'mysql': args.mysql,
              'seeds': seeds, 'cases_per_seed': args.cases, 'meta_cases_per_seed': args.meta_cases,
              'dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT)),
              'results': []}
    env = dict(os.environ, CARGO_TARGET_DIR='target', PINTAIL_ORACLE_MYSQL_IMAGE=f'mysql:{args.mysql}')
    # Do not inherit a developer's evidence/corpus paths or seed overrides.
    for key in list(env):
        if key.startswith(('PINTAIL_FUZZ_', 'PINTAIL_META_')) or key == 'PINTAIL_ORACLE_EVIDENCE':
            del env[key]
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    for name, command, overrides in stages:
        print(f'Running {name}', flush=True)
        started = time.monotonic()
        with (output / f'{name}.log').open('w') as log:
            try:
                completed = subprocess.run(command, cwd=ROOT, env=env | overrides,
                                           stdout=log, stderr=subprocess.STDOUT, timeout=2400)
                code = completed.returncode
            except subprocess.TimeoutExpired:
                log.write('\nStage exceeded 2400 seconds\n')
                code = 124
            except OSError as error:
                log.write(f'\nCannot execute stage: {error}\n')
                code = 127
        content = (output / f'{name}.log').read_text(errors='replace')
        count = passed_tests(content)
        ok = code == 0 and count > 0
        result = {'stage': name, 'status': 'PASS' if ok else 'FAIL', 'exit_code': code,
                  'passed_tests': count, 'seconds': round(time.monotonic() - started, 3),
                  'command': command, 'environment': overrides}
        report['results'].append(result)
        report['status'] = 'RUNNING'
        (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(f'{name}: {result["status"]} ({count} tests, {result["seconds"]}s)', flush=True)
        if not ok:
            print(content[-12000:], flush=True)
    ok = all(result['status'] == 'PASS' for result in report['results'])
    report['status'] = 'PASS' if ok else 'FAIL'
    (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    summary = ['# Extended correctness: ' + report['status'], '',
               f'Commit: `{commit}`; layer: {args.layer}; shard: {args.shard}; MySQL: {args.mysql}', '',
               '| Stage | Status | Rust tests | Seconds |', '| --- | --- | ---: | ---: |']
    for result in report['results']:
        summary.append(f'| {result["stage"]} | {result["status"]} | {result["passed_tests"]} | {result["seconds"]} |')
    summary += ['', 'Rust test counts are not SQL query counts. Consult logs for generated/unique query and comparison counts.',
                'This is supplementary release evidence, not the existing release gate.']
    (output / 'summary.md').write_text('\n'.join(summary) + '\n')
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
