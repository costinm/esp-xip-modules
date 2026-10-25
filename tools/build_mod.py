#!/usr/bin/env python3
"""Directory entry point for the module packer: the Rust module in DIRECTORY (Cargo.toml, one [[bin]], options in [package.metadata] mkmod)."""
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
    parser.add_argument('--slot', type=int, help='the module slot (default: modules/SLOTS): its RAM cell; Xtensa also links code for its 64 KiB window. Not a flash page')
    parser.add_argument('--version', type=int, default=0, help='the manifest version of the module (higher is newer; 0: none)')
    parser.add_argument('--out', type=Path)
    parser.add_argument('--host-metadata', type=Path, help='write canonical native module metadata without compiling an image')
    args = parser.parse_args()
    directory = args.directory.resolve()
    manifest = directory / 'Cargo.toml'
    if not manifest.is_file():
        parser.error('directory must contain a Cargo.toml (C modules were dropped)')
    data = tomllib.loads(manifest.read_text())
    options = shlex.split(data.get('package', {}).get('metadata', {}).get('mkmod', ''))
    bins = data.get('bin', [])
    if len(bins) != 1:
        parser.error('Rust modules must declare exactly one [[bin]]')
    name = bins[0]['name']
    chip_feature = args.chip in data.get('features', {})
    out = (args.out or directory / 'target' / 'out' / args.chip / 'modules').resolve()
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    if not env.get('XIP_RUST_ESP_BIN') and env.get('IDF_PATH'):
        # The SDK harness installs ESP Rust alongside ESP-IDF. This works across calling repositories without knowing their env variable names.
        rust_bin = Path(env['IDF_PATH']).resolve().parent / 'rustup/toolchains/esp/bin'
        if (rust_bin / 'cargo').is_file():
            env['XIP_RUST_ESP_BIN'] = str(rust_bin)
    env.setdefault('CARGO_TARGET_DIR', str(directory / 'target' / 'cargo'))
    features = 'flat-image' + (',' + args.chip if chip_feature else '')
    command = ['python3', str(Path(mkmod.ROOT) / 'tools' / 'mkmod.py'), *options, *(['--version', str(args.version)] if args.version else []),
               '--cargo', f'{manifest}:{name}:{features}']
    if args.host_metadata:
        command += ['--host-metadata', str(args.host_metadata)]
    else:
        slot = args.slot if args.slot is not None else mkmod.read_slots().get(directory.name)
        if slot is None:
            parser.error(f'{directory.name} has no slot: add it to modules/SLOTS')
        command += ['--chip', args.chip, '--slot', str(slot), '--elf', str(out / f'{name}.elf'), '--out', str(out / f'{name}.dmod')]
    subprocess.run(command, env=env, check=True)


if __name__ == '__main__':
    main()
