"""Schema projection for versioned input alternatives with multiple required members."""
import json
import re


def alternatives(fields, choices, kind=False):
    def wire(name):
        return re.sub(r'_([a-z])', lambda match: match[1].upper(), name)

    common = ['kind'] if kind else []
    schemas = []
    optional = set()
    for choice in choices:
        members = choice['members']
        forbidden = choice.get('forbid', [])
        optional.update(members)
        if any(not fields.get(name, '').startswith('?') for name in members + forbidden):
            raise ValueError('input alternative members must be optional fields')
        schema = {
            'required': common + [wire(name) for name, field in fields.items()
                                  if not field.startswith('?') or name in members],
        }
        if forbidden:
            schema['not'] = {'anyOf': [{'required': [wire(name)]} for name in forbidden]}
        for boundary in ('since', 'until'):
            if boundary in choice:
                schema['x-' + boundary] = choice[boundary]
        schemas.append(schema)
    return f'#[schemars(extend("oneOf" = {json.dumps(schemas)}))]', optional
