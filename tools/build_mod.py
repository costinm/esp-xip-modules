#!/usr/bin/env python3
"""Directory entry point for the common module packer; no firmware protocol logic."""
import argparse
import os
from pathlib import Path
import shlex
import subprocess
import tomllib

import mkmod


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--chip', default='esp32c6', choices=tuple(mkmod.CHIPS))
    parser.add_argument('--slots', type=Path)
    parser.add_argument('--slot')
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    directory = args.directory.resolve()
    manifest = directory / 'Cargo.toml'
    name = directory.name
    rust = manifest.is_file()
    options = []
    chip_feature = False
    if rust:
        data = tomllib.loads(manifest.read_text())
        options = shlex.split(data.get('package', {}).get('metadata', {}).get('mkmod', ''))
        bins = data.get('bin', [])
        if len(bins) != 1:
            parser.error('Rust modules must declare exactly one [[bin]]')
        name = bins[0]['name']
        chip_feature = args.chip in data.get('features', {})
    else:
        for source in sorted(directory.glob('*.c')):
            for line in source.read_text().splitlines():
                if 'mkmod:' in line:
                    options = shlex.split(line.split('mkmod:', 1)[1].split('*/', 1)[0])
                    break
            if options:
                break
    slot = name.removeprefix('mod_')
    if '--slot' in options:
        index = options.index('--slot')
        slot = options[index + 1]
        del options[index:index + 2]
    slot = args.slot or slot
    layout = mkmod.slots(str(args.slots)) if args.slots else mkmod.slots()
    if slot not in layout:
        parser.error(f'no slot {slot!r}; supply the product slot table with --slots')
    out = (args.out or directory / 'target' / 'out' / args.chip / 'modules').resolve()
    out.mkdir(parents=True, exist_ok=True)
    # A module can inspect its product layout without importing host code.
    # Keep order: it is also the existing loader state-log module index order.
    layout_file = out / f'{name}.slots'
    layout_file.write_text(''.join(f'{n}\t{tag}\t{offset}\t{size}\n'
                                  for n, (tag, offset, size) in layout.items()))
    env = dict(os.environ)
    if rust and not env.get('XIP_RUST_ESP_BIN') and env.get('IDF_PATH'):
        # The SDK harness installs ESP Rust alongside ESP-IDF. This works
        # across calling repositories without knowing their env variable names.
        rust_bin = Path(env['IDF_PATH']).resolve().parent / 'rustup/toolchains/esp/bin'
        if (rust_bin / 'cargo').is_file():
            env['XIP_RUST_ESP_BIN'] = str(rust_bin)
    env['XIP_SLOT_LAYOUT'] = str(layout_file)
    env['XIP_SLOT'] = slot
    env.setdefault('CARGO_TARGET_DIR', str(directory / 'target' / 'cargo'))
    command = ['python3', str(Path(mkmod.ROOT) / 'tools' / 'mkmod.py'),
               '--chip', args.chip, '--slot', slot, *options,
               '--elf', str(out / f'{name}.elf'), '--out', str(out / f'{name}.dmod')]
    if args.slots:
        command += ['--slots', str(args.slots.resolve())]
    if rust:
        features = 'flat-image' + (',' + args.chip if chip_feature else '')
        command += ['--cargo', f'{manifest}:{name}:{features}']
    else:
        sources = sorted(directory.glob('*.c'))
        if not sources:
            parser.error('directory must contain Cargo.toml or C module sources')
        command += [str(p) for p in sources]
    subprocess.run(command, env=env, check=True)


if __name__ == '__main__':
    main()
