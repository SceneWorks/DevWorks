"""Publish the approved routing amendments from their tracked plans."""
import json
from pathlib import Path


def apply_auto_routing_requirements(router):
    root = Path(__file__).resolve().parents[2]
    plans = [json.loads((root / 'docs/planning' / name).read_text()) for name in
             ('auto-routing-plan.json', 'latest-model-plan.json')]
    assert all(p['status'].startswith('Approved') for p in plans), 'Routing amendment needs approval'
    for change in [c for p in plans for c in p['requirements_delta']]:
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
    router['blocks'].append(('p', 'Latest model acceptance: hourly complete inventories promote family aliases by numeric version, retain missing models as unavailable, and restore returning models without changing user enablement. New tier assignments and the pinless Low classifier use latest-family/custom members; eligible concrete chat pins and known request resolutions survive promotion and restart. ROUTE-25–27 are tracked by DW-108 with SystemOne and Auto integration in DW-22/DW-107; DW-61 reuses their native evidence. Failed or partial lists are not removals.'))
