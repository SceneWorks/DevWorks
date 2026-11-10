#!/usr/bin/env python3
"""Fail a pull request whose hand-written diff exceeds the size limit.

Counts added + deleted lines between the merge base of BASE and HEAD.
Files marked `linguist-generated` in .gitattributes and binary files are
not counted, so generated contracts, recorded validation output and
lockfiles never push a PR over the limit.

Usage: check.py BASE HEAD [--limit N]
Exit 0 within the limit, 1 over it, 2 on a usage or git error.
Standard library only.
"""
import argparse
import subprocess
import sys

DEFAULT_LIMIT = 5000


def git(*args, stdin=None):
    return subprocess.run(['git', *args], input=stdin, capture_output=True,
                          text=True, check=True).stdout


def generated(paths):
    if not paths:
        return set()
    out = git('check-attr', '--stdin', '-z', 'linguist-generated',
              stdin='\0'.join(paths) + '\0')
    fields = out.split('\0')
    return {fields[i] for i in range(0, len(fields) - 2, 3)
            if fields[i + 2] in ('set', 'true')}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('base')
    ap.add_argument('head')
    ap.add_argument('--limit', type=int, default=DEFAULT_LIMIT)
    a = ap.parse_args()
    try:
        numstat = git('diff', '--numstat', '-z', '--no-renames',
                      f'{a.base}...{a.head}')
    except subprocess.CalledProcessError as e:
        print(e.stderr, file=sys.stderr)
        return 2
    rows = []
    for rec in numstat.split('\0'):
        if not rec:
            continue
        added, deleted, path = rec.split('\t', 2)
        rows.append((added, deleted, path))
    skip = generated([p for _, _, p in rows])
    counted, excluded, files = 0, 0, []
    for added, deleted, path in rows:
        if added == '-':
            continue  # binary
        n = int(added) + int(deleted)
        if path in skip:
            excluded += n
        else:
            counted += n
            files.append((n, path))
    print(f'hand-written lines changed: {counted} (limit {a.limit}); '
          f'generated lines not counted: {excluded}')
    if counted <= a.limit:
        return 0
    print('Largest hand-written changes:')
    for n, path in sorted(files, reverse=True)[:15]:
        print(f'  {n:7d}  {path}')
    print(f'\nThis PR changes more than {a.limit} hand-written lines. Split it into '
          'smaller PRs (one CodeGraph item each), or mark generated outputs '
          'linguist-generated in .gitattributes. A PR that genuinely cannot be '
          'split can carry the `large-pr-approved` label.')
    return 1


if __name__ == '__main__':
    sys.exit(main())
