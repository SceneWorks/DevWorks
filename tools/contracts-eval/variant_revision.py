"""Whole-union-variant introductions, shared by the native and probe generators.

A later variant cannot be projected into an earlier alternative. Keep the value intact and
reject its negotiated revision before effects. This is distinct from optional variant members.
"""
import re


def tokens(value):
    return set(re.findall(r'[A-Za-z_][A-Za-z0-9_]*', value))


def containing(types):
    found = set()
    for name, definition in types.items():
        introduced = definition.get('introducedVariants', {})
        if introduced:
            if definition['kind'] != 'union':
                raise ValueError(f'{name}: introducedVariants requires a union')
            for variant, revision in introduced.items():
                if variant not in definition['variants'] or type(revision) is not int or revision < 2:
                    raise ValueError(f'{name}: invalid introduced variant {variant}')
            found.add(name)
    while True:
        grown = found | {name for name, d in types.items() if any(
            tokens(t) & found for t in (d['fields'].values() if d['kind'] == 'record' else
                [t for fs in d['variants'].values() for t in fs.values()] if d['kind'] == 'union' else []))}
        if grown == found:
            return found
        found = grown


def calls(fields, found, prefix='self.'):
    return ''.join(f'{prefix}{name}.check_revision(revision)?;' for name, value in fields.items() if tokens(value) & found)


def method(definition, found):
    if definition['kind'] == 'record':
        body = calls(definition['fields'], found)
    else:
        arms = [f'Self::{v}{{..}} if revision<{revision}=>{{return Err("union variant predates its introduction: {v}".into());}},'
                for v, revision in definition.get('introducedVariants', {}).items()]
        for variant, fields in definition['variants'].items():
            nested = {name: value for name, value in fields.items() if tokens(value) & found}
            if nested:
                arms.append(f'Self::{variant}{{{",".join(nested)},..}}=>{{{calls(nested, found, "")}}},')
        if not arms:
            return ''
        body = 'match self {' + ''.join(arms) + '_=>{}}'
    return f'fn check_revision(&self,revision:u32)->Result<(),String>{{{body}Ok(())}}' if body else ''


def extend(implementation, definition, found):
    check = method(definition, found)
    return implementation[:-1] + check + '}' if check else implementation
