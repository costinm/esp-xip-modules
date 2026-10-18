#!/usr/bin/env python3
"""Runtime/build code must not depend on esp-wifi-loader. External signing and
upload scripts are deliberately outside this audit."""
from pathlib import Path
import re
import tomllib
import unittest
ROOT = Path(__file__).resolve().parent.parent
class Dependencies(unittest.TestCase):
    def test_runtime_manifests_have_no_loader_dependency(self):
        for base in ('crates', 'modules'):
            for path in (ROOT/base).rglob('Cargo.toml'):
                if 'target' in path.parts:
                    continue
                manifest = tomllib.loads(path.read_text())
                groups = [manifest] + list(manifest.get('target', {}).values())
                for group in groups:
                    for kind in ('dependencies', 'build-dependencies', 'dev-dependencies'):
                        for name, value in group.get(kind, {}).items():
                            self.assertNotIn('wifi-loader', name, str(path))
                            self.assertNotIn('wifi-loader', str(value), str(path))
    def test_runtime_source_and_container_build_do_not_link_loader(self):
        patterns = [r'\buse\s+esp_wifi_loader\b', r'\besp_wifi_loader_\w+\s*\(',
                    r'#\s*include\s*[<"]esp_wifi_loader', r'COPY\s+--from=loader']
        for base in ('components', 'crates', 'main'):
            for path in (ROOT/base).rglob('*'):
                if not path.is_file() or 'target' in path.parts or path.suffix not in ('.rs','.c','.h','.txt'):
                    continue
                for pattern in patterns:
                    self.assertIsNone(re.search(pattern, path.read_text()), str(path))
        self.assertNotIn('--from=loader', (ROOT/'Dockerfile').read_text())
    def test_console_works_without_loader_tools(self):
        source = (ROOT/'tools/console.py').read_text()
        self.assertNotIn('import reset', source)
        self.assertNotIn('ESP_WIFI_LOADER_DIR', source)
if __name__ == '__main__':
    unittest.main()
