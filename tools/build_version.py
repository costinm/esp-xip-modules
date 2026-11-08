"""Allocate uint32 image versions using UTC minutes and a persistent local counter."""
from datetime import datetime, timezone
import fcntl
import os
from pathlib import Path

EPOCH = datetime(2026, 1, 1, tzinfo=timezone.utc)
MAX_VERSION = 0xffffffff


def reserve(root, component, chip, requested=None, *, minimum=0, now=None):
    """Reserve before compiling; failed builds may leave gaps, never reused numbers."""
    for value in (requested, minimum):
        if value is not None and not 0 <= value <= MAX_VERSION:
            raise ValueError('version must be a 32-bit unsigned number')
    path = Path(root) / 'target' / 'build-versions' / f'{component}-{chip}'
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+', encoding='ascii') as counter:
        fcntl.flock(counter, fcntl.LOCK_EX)
        counter.seek(0)
        previous = int(counter.read().strip() or '0')
        if not 0 <= previous <= MAX_VERSION:
            raise ValueError(f'invalid version counter: {path}')
        previous = max(previous, minimum)
        minute = int(((now or datetime.now(timezone.utc)) - EPOCH).total_seconds() // 60)
        version = requested if requested is not None else max(1, minute, previous + 1)
        if version > MAX_VERSION:
            raise ValueError(f'version counter exhausted: {path}')
        counter.seek(0)
        counter.truncate()
        counter.write(f'{max(previous, version)}\n')
        counter.flush()
        os.fsync(counter.fileno())
        return version
