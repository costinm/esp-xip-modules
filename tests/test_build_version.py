"""Monotonic build versions, independent of the wall clock and concurrent builds."""
from concurrent.futures import ThreadPoolExecutor
from datetime import timedelta
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tools'))
import build_version as versions


class BuildVersions(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)

    def reserve(self, requested=None, **kwargs):
        kwargs.setdefault('now', versions.EPOCH + timedelta(minutes=100))
        return versions.reserve(self.directory.name, 'mod_flash', 'esp32c6', requested, **kwargs)

    def test_minutes_and_repeated_builds_with_clock_rollback(self):
        self.assertEqual(self.reserve(), 100)
        self.assertEqual(self.reserve(), 101)
        self.assertEqual(self.reserve(now=versions.EPOCH), 102)
        self.assertEqual(self.reserve(now=versions.EPOCH + timedelta(minutes=200)), 200)

    def test_explicit_versions_and_existing_image_floor(self):
        self.assertEqual(self.reserve(300), 300)
        self.assertEqual(self.reserve(0), 0)
        self.assertEqual(self.reserve(2), 2)
        self.assertEqual(self.reserve(), 301)
        self.assertEqual(self.reserve(minimum=500), 501)

    def test_component_and_chip_counters_are_independent(self):
        self.reserve(300)
        for component, chip in [('mod_other', 'esp32c6'), ('mod_flash', 'esp32s3')]:
            self.assertEqual(versions.reserve(self.directory.name, component, chip, now=versions.EPOCH), 1)

    def test_uint32_limits(self):
        for value in [-1, versions.MAX_VERSION + 1]:
            with self.assertRaises(ValueError):
                self.reserve(value)
        self.assertEqual(self.reserve(versions.MAX_VERSION), versions.MAX_VERSION)
        with self.assertRaisesRegex(ValueError, 'exhausted'):
            self.reserve()

    def test_parallel_builds_reserve_distinct_versions(self):
        with ThreadPoolExecutor(max_workers=8) as pool:
            assigned = list(pool.map(lambda _: self.reserve(), range(32)))
        self.assertEqual(sorted(assigned), list(range(100, 132)))

    def test_invalid_counter_is_not_reset(self):
        self.reserve()
        counter = Path(self.directory.name) / 'target/build-versions/mod_flash-esp32c6'
        counter.write_text('broken')
        with self.assertRaises(ValueError):
            self.reserve()
        self.assertEqual(counter.read_text(), 'broken')
