"""Check compose generation and upgrades without installing or starting Docker."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class ComposeTests(unittest.TestCase):
    def rewrite(self, existing=None):
        # Execute the actual compose-writing section, excluding host setup,
        # downloads, Docker commands, health checks and first-boot log output.
        script = Path(__file__).with_name('install.sh').read_text()
        section = script[script.index('$SUDO mkdir -p "$PINTAIL_DIR"'):script.index('# 5. Start it.')]
        with tempfile.TemporaryDirectory() as directory:
            compose = Path(directory) / 'docker-compose.yml'
            if existing is not None:
                compose.write_text(existing)
            subprocess.run(['sh', '-eu', '-c',
                            'say() { printf "%s\\n" "$*"; }\n' + section],
                           env=dict(os.environ, SUDO='', PINTAIL_DIR=directory,
                                    REPO='chittihq/pintail', IMAGE='ghcr.io/chittihq/pintail',
                                    PINTAIL_VERSION='0.1.7', PINTAIL_BIND='127.0.0.1',
                                    PINTAIL_HTTP_PORT='8080', PINTAIL_WIRE_PORT='3306'),
                           check=True, capture_output=True, text=True)
            return compose.read_text()

    def test_fresh_install_labels_selected_release(self):
        compose = self.rewrite()
        self.assertIn('image: ghcr.io/chittihq/pintail:0.1.7', compose)
        self.assertIn('PINTAIL_BUILD_VERSION: "0.1.7"', compose)

    def test_upgrade_moves_image_and_generated_build_version_together(self):
        previous = self.rewrite().replace('0.1.7', '0.1.6')
        upgraded = self.rewrite(previous)
        self.assertNotIn('0.1.6', upgraded)
        self.assertIn('image: ghcr.io/chittihq/pintail:0.1.7', upgraded)
        self.assertIn('PINTAIL_BUILD_VERSION: "0.1.7"', upgraded)

    def test_upgrade_preserves_unrelated_operator_settings(self):
        previous = self.rewrite().replace('0.1.7', '0.1.6').replace(
            'PINTAIL_DATA_DIR: /var/lib/pintail',
            'PINTAIL_DATA_DIR: /custom/data\n      PINTAIL_RELEASE: "custom-release"')
        self.assertEqual(self.rewrite(previous), previous.replace('0.1.6', '0.1.7'))

    def test_upgrade_does_not_add_a_removed_override(self):
        previous = self.rewrite().replace('0.1.7', '0.1.6').replace(
            '      PINTAIL_BUILD_VERSION: "0.1.6"\n', '')
        self.assertEqual(self.rewrite(previous), previous.replace('0.1.6', '0.1.7'))

    def test_upgrade_only_changes_the_generated_service_fields(self):
        previous = '''services:
  before:
    image: ghcr.io/chittihq/pintail:0.1.6
    environment:
      PINTAIL_BUILD_VERSION: "0.1.6"
  pintail:
    image: ghcr.io/chittihq/pintail:0.1.6 # deployed image
    environment:
      PINTAIL_BUILD_VERSION: "0.1.6" # deployed version
      # PINTAIL_BUILD_VERSION: "0.1.6"
      PINTAIL_RELEASE: "0.1.6"
    labels:
      PINTAIL_BUILD_VERSION: "0.1.6"
    # image: ghcr.io/chittihq/pintail:0.1.6
  after:
    image: ghcr.io/chittihq/pintail:0.1.6
    environment:
      PINTAIL_BUILD_VERSION: "0.1.6"
volumes:
  pintail-data:
'''
        expected = previous.replace(
            'image: ghcr.io/chittihq/pintail:0.1.6 # deployed image',
            'image: ghcr.io/chittihq/pintail:0.1.7 # deployed image').replace(
            'PINTAIL_BUILD_VERSION: "0.1.6" # deployed version',
            'PINTAIL_BUILD_VERSION: "0.1.7" # deployed version')
        self.assertEqual(self.rewrite(previous), expected)

    def test_upgrade_leaves_custom_environment_format_alone(self):
        previous = self.rewrite().replace('0.1.7', '0.1.6').replace(
            'PINTAIL_BUILD_VERSION: "0.1.6"', "PINTAIL_BUILD_VERSION: 'custom-release'")
        expected = previous.replace('image: ghcr.io/chittihq/pintail:0.1.6',
                                    'image: ghcr.io/chittihq/pintail:0.1.7')
        self.assertEqual(self.rewrite(previous), expected)

    def test_upgrade_is_idempotent(self):
        compose = self.rewrite()
        self.assertEqual(self.rewrite(compose), compose)


if __name__ == '__main__':
    unittest.main()
