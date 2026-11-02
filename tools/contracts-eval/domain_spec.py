"""Load canonical architecture data from domain-spec.json; edit that JSON, not generated Rust.
No runtime implementation or authoring mutations occur during import.
"""
import json
from pathlib import Path
_SPEC = json.loads(Path(__file__).with_name('domain-spec.json').read_text())
TYPES = _SPEC['types']
OPS = _SPEC['operations']
LEGACY_RESULTS = _SPEC['earlierInputResults']
FAMILY_RULES = _SPEC['familyRules']
assert not set(OPS).intersection(LEGACY_RESULTS)
assert all(name == op['operation'] and op['mode'] in {'Query', 'Command', 'Hybrid'} for name, op in OPS.items())
assert all(op['mode'] != 'Hybrid' or {'queryFields', 'commandFields'} <= op.keys() for op in OPS.values())
