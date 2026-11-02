"""Reviewable MCP publication candidates; no runtime listener or dispatch code."""
from pathlib import Path
import hashlib
import json

ROOT = Path(__file__).resolve().parents[2]
BASE = ROOT / 'docs/architecture'
source = BASE / 'contracts/family-coverage.json'
coverage = json.loads(source.read_text())
# Context families are explicit: a new family cannot become visible by wildcard.
READ_FAMILIES = {
    'projects', 'features', 'issues', 'tasks', 'workflows', 'next_work', 'runs',
    'agents', 'sessions', 'history', 'code', 'indexing', 'analysis', 'schemas',
    'knowledge', 'documents', 'artifacts', 'conventions', 'repositories',
    'help', 'repository_docs',
}
COMMAND_FAMILIES = {
    'features', 'issues', 'tasks', 'workflows', 'knowledge', 'documents',
    'conventions', 'indexing',
}
COMMAND_OPERATIONS = {'code.set_trust', 'repositories.remove_index'}
tools, dispositions = [], []
for family in coverage['families']:
    for operation in family['operations']:
        name = operation['operation']
        for mode in operation['requestModes']:
            allowed = (mode == 'Query' and family['family'] in READ_FAMILIES) or (
                mode == 'Command' and (family['family'] in COMMAND_FAMILIES or name in COMMAND_OPERATIONS))
            if operation['authority'] in ['HumanConfiguration', 'Configuration']:
                allowed = False
                reason = 'Human/configuration authority; native administration, not an incoming context tool.'
            elif not allowed:
                reason = 'Outside code/work/Knowledge context and selected mutation scope; no implicit automation or proxy surface.'
            else:
                reason = 'Eligible scoped read; explicit client selection required.' if mode == 'Query' else 'Explicit command grant plus all existing domain authority and recovery.'
            public_name = 'devworks_' + name.replace('.', '_')
            if len(operation['requestModes']) > 1:
                public_name += '_' + mode.lower()
            dispositions.append({'operation': name, 'mode': mode, 'disposition': 'PublishCandidate' if allowed else 'InternalOnly', 'reason': reason})
            if not allowed:
                continue
            tools.append({
                'name': public_name, 'operation': name, 'mode': mode,
                'inputSchema': 'contracts/' + operation['inputSchema'],
                'inputSelection': operation['selectionConstraint'],
                'resultSchema': 'contracts/' + operation['resultSchema'],
                'resultSelection': operation['resultSelectionConstraint'],
                'authority': operation['authority'], 'effect': operation['effect'],
                'clientGrant': 'SelectedRead' if mode == 'Query' else 'ExplicitCommand',
                'defaultForNewRegistration': False,
                'requiresApplicationIdempotencyKey': mode == 'Command',
                'hostChecks': 'Current registration, scope/view, role, policy, selected operation/mode, revisions and publication revocation; no human impersonation.',
            })
assert len({t['name'] for t in tools}) == len(tools), 'MCP tool name collision'
record = {
    'date': '2026-11-02', 'architectureVersion': '1.1',
    'status': 'Reviewed architecture publication candidates; actual MCP schema projection and client interoperability unimplemented',
    'source': 'contracts/family-coverage.json', 'sourceSha256': hashlib.sha256(source.read_bytes()).hexdigest(),
    'contract': 'mcp-server-contracts.html',
    'argumentProjection': 'Existing selected request schema and exact mode; adapter pins protocol and operation and derives authenticated actor/context. A JSON-RPC ID is not a command idempotency key.',
    'newRegistrationPolicy': 'Disabled and no grants; human approval selects reads/scopes; commands remain separately opt-in.',
    'resourcePolicy': 'Static safe devworks:// templates; object enumeration/read is an authenticated scoped tool/resource projection of these exact queries, preserving the query stamp and citations.',
    'counts': {'catalogOperations': sum(len(f['operations']) for f in coverage['families']),
               'modeDispositions': len(dispositions), 'tools': len(tools),
               'readTools': sum(t['mode'] == 'Query' for t in tools),
               'commandTools': sum(t['mode'] == 'Command' for t in tools)},
    'tools': tools, 'dispositions': dispositions,
}
(BASE / 'mcp-publication-candidates.json').write_text(json.dumps(record, indent=2) + '\n')
print(json.dumps(record['counts']))
