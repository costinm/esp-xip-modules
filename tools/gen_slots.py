#!/usr/bin/env python3
"""Emit a C slot table from the product table accepted by the module packer."""
import argparse
from pathlib import Path
from mkmod import slots
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('source',type=Path);p.add_argument('out',type=Path)
a=p.parse_args();layout=slots(str(a.source))
if not layout: p.error('empty slot table')
a.out.parent.mkdir(parents=True,exist_ok=True)
a.out.write_text('/* Generated from the product slot table. */\n'+''.join(
    f'{{"{name}", {tag}, 0x{off:x}, 0x{size:x}}},\n' for name,(tag,off,size) in layout.items()))
