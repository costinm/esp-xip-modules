"""Check source ownership of Cargo path dependencies; no board or SDK needed."""
import os
from pathlib import Path
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
# The CLI's cross-project protocol test uses the DMesh producer while its
# workspace patches are under development. Production modules remain local.
EXTERNAL_DEV = {
    ('crates/dmesh-cli/Cargo.toml', 'dmesh-discovery'):
        ROOT.parent / 'dmesh/crates/dmesh-discovery',
}


def manifests():
    for base in ('crates', 'modules', 'main-rs'):
        for directory, dirs, names in os.walk(ROOT / base):
            dirs[:] = [d for d in dirs if d not in ('target', '.git', '__pycache__')]
            if 'Cargo.toml' in names:
                yield Path(directory) / 'Cargo.toml'


class Dependencies(unittest.TestCase):
    def test_cargo_paths_follow_ownership(self):
        for path in manifests():
            manifest = tomllib.loads(path.read_text())
            groups = [manifest] + list(manifest.get('target', {}).values())
            for group in groups:
                for kind in ('dependencies', 'build-dependencies', 'dev-dependencies'):
                    for name, value in group.get(kind, {}).items():
                        if not isinstance(value, dict) or 'path' not in value:
                            continue
                        target = (path.parent / value['path']).resolve()
                        if target.is_relative_to(ROOT):
                            continue
                        allowed = EXTERNAL_DEV.get((str(path.relative_to(ROOT)), name))
                        self.assertEqual(kind, 'dev-dependencies', f'{path}: external production dependency {name}')
                        self.assertIsNotNone(allowed, f'{path}: undocumented external dependency {name}')
                        self.assertEqual(target, allowed.resolve(), f'{path}: {name}')


if __name__ == '__main__':
    unittest.main()
