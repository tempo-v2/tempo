#!/usr/bin/env python3
"""Render the managed Lean Daily Queue worker's launchd registration."""
import argparse
from pathlib import Path
import plistlib
import sys


LABEL = 'com.tempo-v2.lean.daily-queue'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('tempo_binary', type=Path)
    parser.add_argument('tempo_database', type=Path)
    args = parser.parse_args()
    binary = args.tempo_binary.resolve(strict=True)
    database = args.tempo_database.resolve(strict=True)
    if not binary.is_file() or not database.is_file():
        parser.error('Provide the installed Tempo binary and its database')
    plistlib.dump({
        'Label': LABEL,
        'ProgramArguments': [str(binary), 'daily-queue-worker', str(database.with_suffix('.dq'))],
        'WorkingDirectory': str(binary.parent.parent),
        'RunAtLoad': True,
        'KeepAlive': True,
        'ThrottleInterval': 5,
        'StandardOutPath': str(database.parent / 'daily-queue.log'),
        'StandardErrorPath': str(database.parent / 'daily-queue-error.log'),
    }, sys.stdout.buffer)


if __name__ == '__main__':
    main()
