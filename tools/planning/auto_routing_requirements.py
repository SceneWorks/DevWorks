"""Publish the approved first-session routing amendment from its tracked plan."""
import json
from pathlib import Path


def apply_auto_routing_requirements(router):
    root = Path(__file__).resolve().parents[2]
    plan = json.loads((root / 'docs/planning/auto-routing-plan.json').read_text())
    assert plan['status'].startswith('Approved'), 'Auto-routing amendment needs approval'
    for change in plan['requirements_delta']:
        requirement = change['source_requirement']
        prefix = requirement + ' [Confirmed]: '
        assert change['after'].startswith(prefix), requirement
        block = ('req', requirement, 'Confirmed', change['after'][len(prefix):])
        matches = [i for i, b in enumerate(router['blocks']) if b[0] == 'req' and b[1] == requirement]
        assert len(matches) <= 1, requirement
        if matches:
            router['blocks'][matches[0]] = block
        else:
            router['blocks'].append(block)
    router['blocks'].append(('p', 'Automatic routing acceptance: new Auto sessions and independent workers classify once through the SystemOne pool or direct Low fallback; explicit selections and upstream Auto/Auto Beta bypass it. Verify durable tier/model affinity across turns, tool continuations, compaction and restart, bounded per-model faults, no recursive classification, no hidden cross-tier fallback, and attributed classifier/generation usage. ROUTE-21–24; CACHE-01–06. Native implementation is tracked by DW-22 and DW-107; planning and pilot judgments are not runtime acceptance.'))
