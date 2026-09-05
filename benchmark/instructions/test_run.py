import unittest
from run import CASES, compare


class BaselineTests(unittest.TestCase):
    def fixture(self):
        return {**{key: "same" for key in ("toolchain", "valgrind", "architecture", "workload_sha256", "threads")}, "instructions": dict.fromkeys(CASES, 100)}

    def test_one_query_regression_cannot_hide_in_an_aggregate_improvement(self):
        before, after = self.fixture(), self.fixture()
        after["instructions"] = dict.fromkeys(CASES, 50)
        after["instructions"]["join"] = 106
        self.assertEqual(compare(before, after, 5), {"join": 6.0})

    def test_environment_changes_and_incomplete_baselines_are_rejected(self):
        before, after = self.fixture(), self.fixture()
        after["toolchain"] = "different"
        with self.assertRaisesRegex(ValueError, "toolchain"):
            compare(before, after, 5)
        after = self.fixture()
        del before["instructions"]["sort"]
        with self.assertRaisesRegex(ValueError, "every workload"):
            compare(before, after, 5)
