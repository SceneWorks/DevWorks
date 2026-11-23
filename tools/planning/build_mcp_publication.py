"""Reviewable MCP publication candidates; no runtime listener or dispatch code."""
from pathlib import Path
import hashlib
import json

ROOT = Path(__file__).resolve().parents[2]
BASE = ROOT / 'docs/architecture'
source = BASE / 'contracts/family-coverage.json'
coverage = json.loads(source.read_text())
# Reviewed operation/mode pairs. Adding an operation to the catalog does not expose it.
# Work queue inspection and schema-to-code references extend the retained context surface.
QUERY_OPERATIONS = set('''
    agents.inbox
    agents.inspect
    analysis.archival_candidates
    analysis.cluster_detail
    analysis.clusters
    analysis.cross_repo_links
    analysis.file_hotspot
    analysis.fleet_health
    analysis.health
    analysis.hotspots
    analysis.metrics
    analysis.security
    analysis.status
    artifacts.export
    artifacts.list
    artifacts.preview
    artifacts.read
    code.architecture
    code.call_path
    code.consumers
    code.data_lineage
    code.dependency
    code.file_impact
    code.graph_schema
    code.graph_scope_status
    code.impact
    code.node_detail
    code.nodes_by_file
    code.overview
    code.publishers
    code.repository_dependencies
    code.search
    code.snippet
    code.source
    code.traverse
    conventions.list
    conventions.read
    conventions.resources
    conventions.search
    conventions.version
    documents.list
    documents.read
    documents.search
    features.get
    features.list
    help.package_guides
    help.read
    help.search
    help.topic_for_screen
    history.command_receipt
    history.events
    history.evidence_links
    history.git_links
    history.operation_status
    history.tool_usage
    history.work_lineage
    indexing.discover
    indexing.runs
    indexing.status
    issues.get
    issues.list
    knowledge.claim_bundle
    knowledge.diagnostics
    knowledge.entity_bundle
    knowledge.entity_graph
    knowledge.expand_frontier
    knowledge.query
    knowledge.read
    knowledge.render_summary
    knowledge.search
    knowledge.subgraph
    knowledge.tagged_lessons
    knowledge.write_status
    next_work.inspect_claim
    next_work.inspect_queue
    projects.get
    projects.list
    repositories.get
    repositories.list
    repositories.nodes
    repositories.readme
    repositories.search
    repositories.summary
    runs.eligibility
    runs.inspect
    schemas.code_references
    schemas.connections
    schemas.objects
    schemas.relationships
    sessions.briefing
    sessions.context_sources
    sessions.get
    sessions.list
    sessions.transcript
    tasks.get
    tasks.list
    workflows.list
'''.split())
COMMAND_OPERATIONS = set('''
    code.set_trust
    conventions.applicability
    conventions.delete
    conventions.delete_resource
    conventions.edit
    conventions.import
    conventions.index
    conventions.move
    conventions.retire
    documents.disable_retrieval
    documents.import
    documents.index
    documents.remove_index
    documents.retry
    features.create
    features.dependencies
    features.edit
    features.edit_plan
    features.prioritize
    features.request_plan_approval
    indexing.cancel
    indexing.configure_sources
    indexing.index
    indexing.refresh_overlay
    indexing.reindex
    indexing.retire_view
    issues.create
    issues.edit
    issues.link
    issues.prioritize
    issues.publish
    knowledge.challenge
    knowledge.delete_by_source
    knowledge.delete_selected
    knowledge.promote
    knowledge.record
    knowledge.relate
    knowledge.retract
    knowledge.revise
    knowledge.scope
    knowledge.tag
    knowledge.verify
    repositories.remove_index
    tasks.acceptance_evidence
    tasks.assign
    tasks.create
    tasks.dependency_links
    tasks.edit
    tasks.move
    tasks.prioritize
    tasks.set_feature
    tasks.sync_plan
    workflows.create_status
    workflows.order
    workflows.reassign_and_remove
    workflows.rename_status
'''.split())
PRIVATE_MODE_REASONS = {
    'documents.move': 'Document transfers require the source and destination project coordinators.',
    'documents.share': 'Document transfers require the source and destination project coordinators.',
    'documents.transfer_propose': 'Only the native source project coordinator can propose a transfer.',
    'documents.transfer_respond': 'Only the native destination project coordinator can decide its need.',
    'documents.transfer_status': 'Private to the authorized native coordinators.',
    'projects.prepare_purge': 'Native project maintenance requires the human confirmation flow.',
    'projects.purge_manifest': 'Private native project purge review and file locations.',
    'projects.purge': 'Native human-only project purge confirmation.',
    'projects.purge_status': 'Private native project purge progress.',
    'agents.post': 'Internal agent coordination can dispatch messages or change work.',
    'agents.receive': 'Internal retained inbox coordination.',
    'agents.ack': 'Internal retained inbox coordination.',
    'agents.history': 'Private agent-to-agent messages and delivery state.',
    'agents.events': 'Private agent-to-agent events and delivery state.',
    'artifacts.tag': 'Artifact organization is outside the selected command set.',
    'knowledge.import_user_data': 'Native import requires the human actor that supplied the retained user data.',
    'skills.set_enabled': 'Native skills configuration.',
    'approvals.review_queue': 'Private human approval queue.',
    'notifications.mark_read': 'Native notification state.',
    'router.embedding_spaces': 'Private provider and embedding configuration.',
    'mcp.disable': 'Private outgoing MCP administration.',
    'mcp.remove': 'Private outgoing MCP administration.',
    'mcp.classify_tool': 'Private outgoing MCP administration.',
    'addons.invoke': 'Private package dispatch.',
    'addons.run_workflow': 'Private package workflow dispatch.',
    'addons.read_content': 'Private package result access.',
    'addons.invocation': 'Private package invocation state.',
    'evals.grade': 'Private evaluation execution.',
    'evals.results': 'Private evaluation results.',
    'watch.create': 'Private watcher control with possible process execution.',
    'watch.list': 'Private watcher state.',
    'watch.inspect': 'Private watcher state.',
    'watch.cancel': 'Private watcher control.',
}
tools, dispositions = [], []
for family in coverage['families']:
    for operation in family['operations']:
        name = operation['operation']
        for mode in operation['requestModes']:
            allowed = name in (QUERY_OPERATIONS if mode == 'Query' else COMMAND_OPERATIONS)
            if operation['authority'] in ['HumanConfiguration', 'Configuration', 'HumanDecision',
                                          'HumanMaintenance', 'HostMaintenance']:
                assert not allowed, (name, mode, operation['authority'])
                reason = PRIVATE_MODE_REASONS.get(
                    name, 'Human/configuration authority; native administration, not an incoming context tool.'
                )
            elif not allowed:
                reason = PRIVATE_MODE_REASONS.get(
                    name, 'Outside code/work/Knowledge context and selected mutation scope; no implicit automation or proxy surface.'
                )
            else:
                reason = 'Eligible scoped read; explicit client selection required.' if mode == 'Query' else 'Explicit command grant plus existing domain authority and recovery.'
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
assert {t['operation'] for t in tools if t['mode'] == 'Query'} == QUERY_OPERATIONS
assert {t['operation'] for t in tools if t['mode'] == 'Command'} == COMMAND_OPERATIONS
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
