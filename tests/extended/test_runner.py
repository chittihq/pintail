import argparse
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from run import passed_tests, plan, positive, seeds_for


class RunnerTests(unittest.TestCase):
    def test_seed_replay_and_disjoint_shards(self):
        seeds = [seed for shard in range(4) for seed in seeds_for('abc123', shard)]
        self.assertEqual(len(seeds), len(set(seeds)))
        self.assertTrue(all(seed & 1 for seed in seeds))
        self.assertEqual(seeds_for('abc123', 2), seeds_for('abc123', 2))
        self.assertNotEqual(seeds_for('abc123', 2)[1], seeds_for('def456', 2)[1])

    def test_filtered_empty_run_cannot_pass(self):
        self.assertEqual(passed_tests('test result: ok. 0 passed; 0 failed;'), 0)
        self.assertEqual(passed_tests('test result: FAILED. 3 passed; 1 failed;'), 0)
        self.assertEqual(passed_tests('test result: ok. 7 passed; 0 failed;'), 7)

    def test_sql_plan_replays_explicit_cases_and_exact_tests(self):
        with tempfile.TemporaryDirectory() as directory:
            stages = plan('sql', 0, 23, 11, [3, 5], Path(directory))
            self.assertEqual(len(stages), 4)
            self.assertTrue(all('--exact' in command for _, command, _ in stages))
            self.assertEqual(stages[1][2]['PINTAIL_FUZZ_SEEDS'], '3,5')
            self.assertEqual(stages[1][2]['PINTAIL_FUZZ_CASES'], '23')
            self.assertEqual(stages[2][2]['PINTAIL_META_CASES'], '11')
            self.assertEqual(len(plan('sql', 1, 23, 11, [7, 9], Path(directory))), 3)

    def test_zero_work_is_rejected(self):
        for value in ['0', '-1', '100001']:
            with self.assertRaises(argparse.ArgumentTypeError):
                positive(value)

    def test_failed_or_empty_stage_fails_run_and_preserves_other_results(self):
        for mode in ['failure', 'empty']:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                fake = root / 'cargo'
                fake.write_text("#!/usr/bin/env python3\nimport sys\n"
                                "bad = 'pintail-sqllogic' in sys.argv\n"
                                "print('test result: ok. %d passed; 0 failed;' % (0 if bad else 1))\n"
                                + ("sys.exit(1 if bad else 0)\n" if mode == 'failure' else ''))
                fake.chmod(0o755)
                result = subprocess.run([sys.executable, str(Path(__file__).with_name('run.py')),
                                         '--layer', 'core', '--output', str(root / 'out')],
                                        env=dict(os.environ, CARGO=str(fake)), capture_output=True)
                self.assertEqual(result.returncode, 1, result.stderr)
                report = json.loads((root / 'out/report.json').read_text())
                self.assertEqual(report['status'], 'FAIL')
                self.assertEqual([r['status'] for r in report['results']], ['FAIL', 'PASS', 'PASS'])
