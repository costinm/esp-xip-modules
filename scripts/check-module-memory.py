#!/usr/bin/env python3
"""Check module state/stack reservation caps."""
import json
from pathlib import Path
import shlex
import sys
import tomllib

root = Path(__file__).resolve().parent.parent
budgets = json.loads((root / 'modules/retained-memory.json').read_text())
errors = []
# C modules (no Cargo.toml) keep their reviewed budget here too; their limits are checked by tools/build_c_mod.py.
seen = {d.name for d in (root / 'modules').glob('mod_*') if d.is_dir()}
for manifest in sorted((root / 'modules').glob('mod_*/Cargo.toml')):
    module = manifest.parent.name
    options = shlex.split(tomllib.loads(manifest.read_text())['package']['metadata']['mkmod'])
    if module not in budgets:
        errors.append(f'{module}: missing reviewed RAM budget')
        continue
    actual = {}
    for key, flag in [('state_bytes', '--state'), ('stack_bytes', '--stack')]:
        try:
            actual[key] = int(options[options.index(flag) + 1])
            if actual[key] > budgets[module][key]:
                errors.append(f'{module}: {key} {actual[key]} exceeds {budgets[module][key]}')
        except (ValueError, IndexError, KeyError):
            errors.append(f'{module}: missing/invalid {flag} or budget')
    print(json.dumps({'module': module, **actual, 'limits': budgets[module]}))
for module in budgets.keys() - seen:
    errors.append(f'{module}: RAM budget has no module')
if errors:
    sys.exit('\n'.join(errors))
