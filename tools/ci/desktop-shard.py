#!/usr/bin/env python3
"""Disjoint complete desktop target coverage under the existing CI job budget."""
import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
BASE = ['cargo', 'test', '-p', 'devworks-desktop', '--locked']


def inventory(targets):
    runnable, examples, docs = [], [], []
    for target in targets:
        kind, name = target['kind'], target['name']
        if kind == ['custom-build']:
            continue  # Cargo builds the package's build script automatically.
        if kind not in (['lib'], ['bin'], ['test'], ['example']):
            raise ValueError(f'Unclassified desktop target: {kind} {name}')
        if target.get('test', True):
            runnable.append((kind[0], name))
        elif kind == ['example']:
            examples.append(name)  # Default cargo test builds examples without running them.
        elif kind != ['lib']:
            raise ValueError(f'Unclassified non-test desktop target: {kind} {name}')
        if target.get('doctest', False):
            if kind != ['lib']:
                raise ValueError(f'Unclassified doctest target: {kind} {name}')
            docs.append(name)
    return sorted(runnable), sorted(examples), sorted(docs)


def plan(targets, shard, count):
    if count < 1 or not 0 <= shard < count:
        raise ValueError('Shard must be within a positive shard count')
    runnable, examples, docs = inventory(targets)
    selected = [target for i, target in enumerate(runnable) if i % count == shard]
    coverage, commands = [], []
    if selected:
        command = BASE.copy()
        for kind, name in selected:
            command += ['--lib'] if kind == 'lib' else ['--' + kind, name]
            coverage.append(f'{kind}:{name}')
        commands.append(command)
    if shard == count - 1:
        if examples:
            command = ['cargo', 'build', '-p', 'devworks-desktop', '--locked', '--profile', 'test']
            for name in examples:
                command += ['--example', name]
                coverage.append('example-build:' + name)
            commands.append(command)
        if docs:
            commands.append(BASE + ['--doc'])
            coverage += ['doc:' + name for name in docs]
    if not commands:
        raise ValueError('Empty desktop shard')
    return {'shard': shard, 'shards': count, 'coverage': coverage, 'commands': commands}


def verify(targets, count):
    runnable, examples, docs = inventory(targets)
    expected = {f'{kind}:{name}' for kind, name in runnable}
    expected.update('example-build:' + name for name in examples)
    expected.update('doc:' + name for name in docs)
    plans = [plan(targets, i, count) for i in range(count)]
    actual = [target for part in plans for target in part['coverage']]
    if len(actual) != len(set(actual)) or set(actual) != expected:
        raise ValueError('Desktop shards must cover every default target exactly once')
    return plans


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--shard', type=int, default=0)
    parser.add_argument('--shards', type=int, default=2)
    parser.add_argument('--verify', action='store_true')
    parser.add_argument('--plan', action='store_true')
    args = parser.parse_args()
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT))
    package = next(p for p in metadata['packages'] if p['name'] == 'devworks-desktop')
    plans = verify(package['targets'], args.shards)
    if args.verify:
        print(json.dumps({'defaultTargets': sum(len(p['coverage']) for p in plans), 'plans': plans}))
        return
    selected = plan(package['targets'], args.shard, args.shards)
    print(json.dumps(selected), flush=True)
    if not args.plan:
        for command in selected['commands']:
            subprocess.run(command, cwd=ROOT, check=True)


if __name__ == '__main__':
    main()
