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
            kind = definition['kind']
            choices = definition.get('variants') if kind == 'union' else definition.get('values')
            if kind not in ('union', 'enum'):
                raise ValueError(f'{name}: introducedVariants requires a union or enum')
            for variant, revision in introduced.items():
                if variant not in choices or type(revision) is not int or revision < 2:
                    raise ValueError(f'{name}: invalid introduced variant {variant}')
            fallbacks = definition.get('fallbackVariants', {})
            if fallbacks and (kind != 'enum' or not all(
                v in introduced and old in choices and old not in introduced
                for v, old in fallbacks.items()
            )):
                raise ValueError(f'{name}: enum fallbacks must name variants available in earlier revisions')
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
    elif definition['kind'] == 'enum':
        guards = [f'if matches!(self, Self::{v}) && revision < {r} {{return Err("enum variant predates its introduction: {v}".into());}}'
                  for v, r in definition.get('introducedVariants', {}).items()]
        body = ''.join(guards)
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


def enum_members(definition, pascal):
    for value in definition['values']:
        since = definition.get('introducedVariants', {}).get(value)
        if since:
            yield f'    #[schemars(extend("x-since" = {since}))]'
        if not value[0].isupper():
            yield f'    #[serde(rename = "{value}")]'
        yield f'    {pascal(value)},'


def enum_projection(name, definition, found):
    since = definition.get('introducedVariants', {})
    fallbacks = definition.get('fallbackVariants', {})
    if not since:
        return ''
    retain = ''.join(f'if matches!(self, Self::{v}) && revision < {since[v]} {{*self = Self::{old};}}'
                     for v, old in fallbacks.items())
    check = method(definition, found)
    parameter = 'revision' if retain else '_revision'
    return f'impl Revisioned for {name} {{fn retain_revision(&mut self, {parameter}:u32){{{retain}}}{check}}}'
