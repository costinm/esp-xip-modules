#!/usr/bin/env python3
"""Run documented C Main E2E scenarios using live devices and dmesh-cli."""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import sys

from harness import CLI, Failure, ROOT, Skip, discover, resolve_cli
from scenarios import CASES


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('action', choices=('list', 'run'))
    p.add_argument('cases', nargs='*', help='scenario names; default: discovery uart-churn echo probe async')
    p.add_argument('--cli', help='explicit dmesh-cli executable (default: scripts/build-cli.sh output)')
    p.add_argument('--target', action='append', default=[], help='live serial path or udp:// endpoint; repeat for pairs')
    p.add_argument('--serial-only', action='store_true', help='do not perform network discovery')
    p.add_argument('--output', type=Path, help='fresh evidence directory (default: target/e2e/TIMESTAMP)')
    p.add_argument('--require', action='store_true', help='any skipped case makes the run fail')
    p.add_argument('--allow-radio', action='store_true', help='include explicitly selected radio transition scenarios')
    p.add_argument('--requests', type=int, default=25)
    p.add_argument('--timeout', type=float, default=25, help='deadline for one control process')
    p.add_argument('--probe-timeout', type=int, default=60)
    p.add_argument('--bytes', type=int, default=65536)
    p.add_argument('--packet-size', type=int, default=512)
    p.add_argument('--rounds', type=int, default=1)
    p.add_argument('--directions', nargs='+', choices=('upload', 'download', 'both'), default=['both'])
    p.add_argument('--bearer', choices=('uart', 'udp', 'tcp'), default='uart')
    p.add_argument('--backend', choices=('raw', 'socket'), help='require this preconfigured Wi-Fi raw_rx backend')
    p.add_argument('--transition-timeout', type=float, default=20)
    p.add_argument('--package', type=Path, help='newer signed module WLU1 package for flash validation/install')
    p.add_argument('--write', action='store_true', help='flash: commit module, reset and verify cold rescan/activation')
    return p


def main(argv=None):
    p = parser()
    args = p.parse_args(argv)
    if args.action == 'list':
        for name, (_, radio, install) in CASES.items():
            print(f'{name:14} tests/e2e/scenarios/{name}.md' +
                  (' [--allow-radio]' if radio else '') + (' [--package; --write to install/reset]' if install else ''))
        return 0
    names = args.cases or ['discovery', 'uart-churn', 'echo', 'probe', 'async']
    if any(n not in CASES for n in names):
        p.error('unknown scenario; use list')
    for key in ('requests', 'timeout', 'probe_timeout', 'bytes', 'packet_size', 'rounds', 'transition_timeout'):
        if getattr(args, key) <= 0:
            p.error(key.replace('_', '-') + ' must be positive')
    if args.probe_timeout > 300 or args.packet_size > 1500:
        p.error('probe-timeout must be <=300 and packet-size <=1500')
    if args.write and ('flash' not in names or len(args.target) != 1 or not args.package):
        p.error('--write requires flash, --package and exactly one explicit --target')
    output = args.output or ROOT / 'target/e2e' / datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
    rows = []
    created = False
    try:
        cli = CLI(resolve_cli(args.cli), output, args.timeout)
        created = True
        devices = discover(cli, args.target, network=not args.serial_only)
        for name in names:
            case, radio, _ = CASES[name]
            if name == 'discovery':
                groups = [devices]
            elif name in ('now', 'nan-wake'):
                groups = [[d for d in devices if d.serial()][:2]]
            else:
                groups = [[d] for d in devices]
            if not groups:
                groups = [[]]
            for group in groups:
                row = dict(case=name, devices=[d.identity for d in group], status='PASS')
                try:
                    if not group:
                        raise Skip('no discovered C Main devices')
                    if radio and not args.allow_radio:
                        raise Skip('radio changes require --allow-radio')
                    case(cli, group, args)
                except Skip as error:
                    row.update(status='SKIP', reason=str(error))
                except Exception as error:
                    row.update(status='FAIL', reason=str(error))
                rows.append(row)
                print(json.dumps(row), flush=True)
    except Exception as error:
        row = dict(case='setup', status='FAIL', reason=str(error))
        rows.append(row)
        print(json.dumps(row), flush=True)
    counts = {s: sum(r['status'] == s for r in rows) for s in ('PASS', 'FAIL', 'SKIP')}
    summary = dict(counts=counts, results=rows, evidence=str(output))
    if created:
        (output / 'results.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(dict(summary=counts, evidence=str(output))), flush=True)
    if counts['FAIL'] or (args.require and counts['SKIP']):
        return 1
    return 0 if counts['PASS'] else 77


if __name__ == '__main__':
    sys.exit(main())
