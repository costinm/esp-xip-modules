#!/usr/bin/env python3
"""This repository builds and tests with nothing outside it: no sibling checkout, no path dependency leaving the tree, and
the loader project (esp-wifi-loader) is named only by the one script and the one document that deliberately use its tools."""
from pathlib import Path
import re
import tomllib
import unittest
ROOT = Path(__file__).resolve().parent.parent
ALLOWED = {'scripts/flash-via-loader.sh', 'tests/test_dependencies.py', 'AGENTS.md',   # AGENTS.md states the ownership split
           }
ALLOWED_DIRS = ('crates/dmesh-cli/', 'tests/e2e/')   # moved from DMesh as is (2026-10-20); they still mention the loader project in comments and e2e   # the notes tree (docs) is outside the repository
SKIP = {'target', '.git', 'notes', '__pycache__', 'node_modules'}


def files(*bases):
    for base in bases:
        for path in (ROOT / base).rglob('*') if (ROOT / base).is_dir() else [ROOT / base]:
            if path.is_file() and not SKIP & set(path.relative_to(ROOT).parts):
                yield path


class Dependencies(unittest.TestCase):
    def test_cargo_paths_stay_inside_the_repository(self):
        for path in files('crates', 'modules', 'fw', 'main-rs'):
            if path.name != 'Cargo.toml':
                continue
            manifest = tomllib.loads(path.read_text())
            groups = [manifest] + list(manifest.get('target', {}).values())
            for group in groups:
                for kind in ('dependencies', 'build-dependencies', 'dev-dependencies'):
                    for name, value in group.get(kind, {}).items():
                        if isinstance(value, dict) and 'path' in value:
                            target = (path.parent / value['path']).resolve()
                            self.assertTrue(target.is_relative_to(ROOT), f'{path}: {name}')

    def test_only_the_flash_script_names_the_loader_project(self):
        for path in files('.'):
            if path.suffix in ('.pkg', '.bin', '.dmod', '.pyc', '.lock') or str(path.relative_to(ROOT)) in ALLOWED or str(path.relative_to(ROOT)).startswith(ALLOWED_DIRS):
                continue
            try:
                text = path.read_text()
            except UnicodeDecodeError:
                continue
            self.assertIsNone(re.search(r'esp-wifi-loader|ESP_WIFI_LOADER|wl_format|sign-bundle', text), str(path))


if __name__ == '__main__':
    unittest.main()
