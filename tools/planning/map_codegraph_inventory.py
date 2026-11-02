"""Assign reviewed native destinations to the frozen source surface inventory.

The rules account for public aliases and supporting indexing stages. They are
design bindings, not executable native APIs or evidence of runtime parity.
"""
import argparse
import json
from collections import Counter
from pathlib import Path


def main(source, output):
    data = json.loads(source.read_text())
    def result(group, *operations, disposition="native", reason="Retain behavior through the shared native operation boundary"):
        return dict(group=group, targetOperations=list(operations), disposition=disposition, reason=reason, acceptanceCase="CAP-"+group)
    def classify(e):
        p=e['source']['path']; f=p.split('/')[-1]; n=e.get('method',e['name']); kind=e['kind']
        if kind=='mcp' and f=='McpHubServer.cs':
            if e['name'].startswith('mysql_'):
                return result('DATABASE', 'database.read' if e['name']=='mysql_readonly_query' else 'schemas.objects', disposition='adapt',reason='Retain MySQL tools in separately scoped native database adapters; no legacy hub/shared-account dependency')
            if e['name']=='mcp_hub_catalog':return result('MCP','mcp.discover',disposition='adapt',reason='Local tool/server catalog replaces remote hub hosting')
            return result('EXCLUDED',disposition='excluded',reason='OP-03 explicitly excludes legacy Shortcut and RabbitMQ integrations')
        if kind=='mcp' and f=='OverlayMcpServer.cs':return result('OVERLAY','indexing.refresh_overlay' if 'request' in e['name'] else 'indexing.status')
        if kind=='mcp' and f=='MemoryMcpServer.cs':
            op={'store_memory_v2':'record','get_memory_write_status':'write_status','query_memory':'query','search_memory':'search','get_memory_subgraph':'subgraph','get_entity_bundle':'entity_bundle','get_claim_bundle':'claim_bundle','expand_memory_frontier':'expand_frontier','render_memory_summary':'render_summary'}[e['name']]
            return result('KNOWLEDGE','knowledge.'+op,disposition='adapt',reason='Retain structured entities/claims/evidence and traversal within unified tagged Knowledge; no legacy corpus migration')
        if kind=='mcp' and f=='ConsolidatedMcpServer.cs':
            table={'codegraph_search':('GRAPH','code.search','code.graph_schema'),'rag_search':('CONVENTIONS','conventions.search'),'graph_trace':('GRAPH','code.call_path','code.data_lineage','code.consumers','code.publishers','code.impact'),'graph_source':('SOURCE','code.source','code.snippet'),'repository_report':('REPOSITORIES','repositories.search','code.architecture','analysis.health','analysis.fleet_health','analysis.archival_candidates'),'graph_cluster':('ANALYSIS','analysis.clusters','analysis.cluster_detail'),'storage_table':('DATABASE','schemas.connections','schemas.objects'),'convention':('CONVENTIONS','conventions.list','conventions.read'),'memory_read':('KNOWLEDGE','knowledge.query','knowledge.search','knowledge.subgraph','knowledge.entity_bundle','knowledge.claim_bundle','knowledge.expand_frontier','knowledge.render_summary'),'memory_store':('KNOWLEDGE','knowledge.record'),'memory_diagnostics':('KNOWLEDGE','knowledge.write_status')}
            g,*ops=table[e['name']];return result(g,*ops,disposition='alias',reason='Source intent-family operation modes alias native contracts; preserve every declared operation mode')
        if (kind=='mcp' and f=='CodeGraphMcpServer.cs') or kind=='assistant_tool':
            table={'get_graph_schema':('GRAPH','code.graph_schema'),'list_repositories':('REPOSITORIES','repositories.list'),'search_repositories':('REPOSITORIES','repositories.search'),'list_schemas':('DATABASE','schemas.connections'),'get_schema_catalog':('DATABASE','schemas.objects'),'search_graph':('GRAPH','code.search'),'get_service_summary':('REPOSITORIES','repositories.summary'),'trace_call_path':('GRAPH','code.call_path'),'trace_data_lineage':('GRAPH','code.data_lineage'),'find_consumers':('GRAPH','code.consumers'),'find_publishers':('GRAPH','code.publishers'),'get_architecture':('GRAPH','code.architecture'),'get_repository_health':('ANALYSIS','analysis.health'),'get_fleet_health':('ANALYSIS','analysis.fleet_health'),'find_archival_candidates':('ANALYSIS','analysis.archival_candidates'),'get_code_snippet':('SOURCE','code.snippet'),'read_node_source':('SOURCE','code.source'),'list_conventions':('CONVENTIONS','conventions.list'),'get_service_clusters':('ANALYSIS','analysis.clusters'),'get_cluster_detail':('ANALYSIS','analysis.cluster_detail'),'analyze_impact':('GRAPH','code.impact'),'analyze_impact_file':('GRAPH','code.file_impact'),'get_repo_dependencies':('GRAPH','code.repository_dependencies'),'file_hotspot':('ANALYSIS','analysis.file_hotspot'),'get_convention':('CONVENTIONS','conventions.read'),'search_conventions':('CONVENTIONS','conventions.search')}
            g,op=table[e['name']];return result(g,op)
        if kind=='work_tool' or (kind=='http' and f in ['WorkController.cs','WorkController.Dreaming.cs']):
            aliases={'Create':'create_work','Find':'find_work','Next':'next_work','SyncPlan':'sync_plan','StartSession':'start_session','Record':'record','Recall':'recall','ImportKnowledge':'import_knowledge','Promote':'promote','Challenge':'challenge','Lessons':'lessons','DreamPropose':'dream_propose','DreamReview':'dream_review','ReviewQueue':'review_queue','GetKnowledge':'recall','EndSession':'end_session','Briefing':'get_briefing','Post':'post','Ack':'ack','Inbox':'inbox','Acquire':'acquire','RenewHold':'renew_hold','ReleaseHold':'release','Changes':'changes','Get':'get_work','Update':'update_work','RunDream':'dream_run','DreamRuns':'dream_runs','DreamRun':'dream_runs','RevertDreamRun':'dream_revert','DreamSettings':'dream_settings','SaveDreamSettings':'dream_settings','DreamInstructions':'dream_instructions','SaveDreamInstructions':'dream_instructions'}
            op=e['name'] if kind=='work_tool' else aliases[n]
            if op.startswith('dream_'):return result('DREAMING','dreaming.'+op[6:])
            if op in ['record','recall','import_knowledge','promote','challenge','lessons']:return result('KNOWLEDGE','knowledge.'+({'recall':'search','import_knowledge':'import_user_data','lessons':'tagged_lessons'}.get(op,op)))
            if op=='review_queue':return result('DECISIONS','approvals.list')
            if op in ['post','inbox','ack']:return result('SESSIONS','agents.'+{'post':'messages','inbox':'inbox','ack':'acknowledge'}[op])
            if op in ['acquire','renew_hold','release']:return result('RESOURCES','resources.'+{'acquire':'acquire','renew_hold':'renew','release':'release'}[op],disposition='adapt',reason='Enforced canonical resources replace advisory project-local holds')
            if op in ['start_session','end_session','get_briefing']:return result('SESSIONS','sessions.'+{'start_session':'start','end_session':'end','get_briefing':'briefing'}[op])
            if op=='next_work':return result('WORK','next_work.claim_project_queue','next_work.claim_feature_task')
            if op=='changes':return result('HISTORY','history.events')
            if op=='sync_plan':return result('WORK','features.edit_plan','tasks.sync_plan')
            verb={'create_work':'create','get_work':'get','find_work':'list','update_work':'edit'}[op]
            return result('WORK',*[x+'.'+verb for x in ['projects','features','tasks','issues']])
        if kind=='job':
            table={'DreamProjectsJob':('DREAMING','dreaming.run'),'DiscoverRepositoriesJob':('INDEXING','indexing.discover'),'ReIndexAllRepositoriesJob':('INDEXING','indexing.reindex'),'ProcessBatchAnalysisJob':('ANALYSIS','analysis.process_batch'),'LinkAndDetectJob':('LINKING','analysis.link_and_detect'),'DetectCommunitiesJob':('ANALYSIS','analysis.community_analysis'),'RegenerateMcpDocsJob':('DOCS','help.generate_capability_docs'),'IngestConventionEmbeddingsJob':('CONVENTIONS','conventions.index'),'WorkImportJob':('IMPORT','imports.apply'),'AssistantRetentionCleanupJob':('MAINTENANCE','maintenance.prune_diagnostics'),'IndexerRunRetentionJob':('MAINTENANCE','maintenance.prune_index_runs'),'PruneWorkMessagesJob':('MAINTENANCE','maintenance.prune_messages'),'PruneWorkSessionsJob':('MAINTENANCE','maintenance.prune_sessions')}
            g,op=table[n];return result(g,op,disposition='adapt',reason='Native durable job; legacy automatic destructive retention is not imported. Work import uses selected existing CodeGraph records, not Shortcut/Kanban live adapters')
        if kind=='consumer':
            table={'WikiPageChangedConsumer':('CONVENTIONS','conventions.index'),'AnalysisBatchSubmittedConsumer':('ANALYSIS','analysis.process_batch'),'AnalysisSynthesisCompletedConsumer':('DOCS','repository_docs.generate'),'ProcessRepositoryConsumer':('INDEXING','indexing.index'),'ProjectAnalysisResultsProcessedConsumer':('ANALYSIS','analysis.synthesize'),'RepositoryIndexingCompletedConsumer':('LINKING','analysis.cross_repo_links'),'RepositoryRemovedConsumer':('REPOSITORIES','repositories.remove_index'),'StoreMemoryClaimsConsumer':('KNOWLEDGE','knowledge.record'),'LlmUsageRecordedConsumer':('USAGE','router.usage'),'McpToolInvocationRecordedConsumer':('USAGE','history.tool_usage')}
            g,op=table[n];return result(g,op,disposition='adapt',reason='Atomic local events/queue entries and native consumers replace MassTransit/RabbitMQ delivery')
        if kind in ['extractor','semantic_adapter','index_stage']:
            return result('LANGUAGES' if kind!='index_stage' else 'LINKING','indexing.index' if kind!='index_stage' else 'analysis.cross_repo_links',disposition='support',reason='Retain extraction/linking behavior and capability coverage; replace implementation/runtime as appropriate. Supporting type is not an independent public operation')
        if kind=='resource':
            return result('CONVENTIONS' if 'conventions' in e['name'] else 'REPOSITORIES' if 'repositories' in e['name'] else 'MCP','conventions.read' if 'conventions' in e['name'] else 'repositories.summary' if 'repositories' in e['name'] else 'mcp.capabilities',disposition='adapt',reason='Resource data retained through native query/Help contracts; DevWorks does not host the legacy MCP server')
        if kind=='minimal_http' or f in ['AuthConfigController.cs','UserMcpTokensController.cs','MemoryWriteHealthController.cs'] or (f=='HealthController.cs'):
            return result('SERVER_REPLACEMENT','workspace.diagnostics',disposition='replace',reason='Single-user desktop diagnostics/permissions replace server readiness, web sign-in, service health and issued MCP-server tokens. No public app/MCP server')
        if kind=='http':
            if f=='AdminController.cs':
                if n in ['ListAdmins','AddAdmin','RemoveAdmin']:return result('SERVER_REPLACEMENT','settings.read_effective',disposition='replace',reason='Single desktop user has no web administrator membership')
                return result('QUEUE' if 'Job' in n else 'ROLES','queue.inspect' if n=='GetJobQueue' else 'queue.resolve' if n=='DismissJob' else 'roles.read' if n=='ListPrompts' else 'roles.propose')
            if f=='IndexerController.cs' or f=='SettingsController.cs' and n in ['ProcessRepositories','ReIndexAllRepos','Link','DetectCommunities','LinkAndDetect','ProcessBatchAnalysis','Discover','RegenerateMcpDocs']:
                table={'ProcessRepositories':('INDEXING','indexing.index'),'ReIndexAll':('INDEXING','indexing.reindex'),'ReIndexAllRepos':('INDEXING','indexing.reindex'),'Discover':('INDEXING','indexing.discover'),'Link':('LINKING','analysis.cross_repo_links'),'DetectCommunities':('ANALYSIS','analysis.community_analysis'),'LinkAndDetect':('LINKING','analysis.link_and_detect'),'ProcessBatchAnalysis':('ANALYSIS','analysis.process_batch'),'CreateBranchOverlay':('OVERLAY','indexing.refresh_overlay'),'GetBranchOverlay':('OVERLAY','indexing.status'),'ListRuns':('INDEXING','indexing.runs'),'GetRun':('INDEXING','indexing.status'),'CancelRun':('INDEXING','indexing.cancel'),'SyncSchema':('DATABASE','schemas.refresh'),'SyncAllSchemas':('DATABASE','schemas.refresh'),'DreamProjects':('DREAMING','dreaming.run'),'ReAnalyze':('ANALYSIS','analysis.reanalyze'),'RegenerateMcpDocs':('DOCS','help.generate_capability_docs')}
                g,op=table[n];return result(g,op)
            if f=='SettingsController.cs':
                if 'Schedule' in n:return result('JOBS','jobs.'+{'ListSchedules':'list','GetSchedule':'inspect','CreateSchedule':'create','UpdateSchedule':'edit','DeleteSchedule':'delete','RunScheduleNow':'run_now','EnableSchedule':'enable','DisableSchedule':'disable'}[n])
                return result('INDEXING','workspace.diagnostics' if n=='GetDatabaseHealth' else 'indexing.configure_sources')
            if f in ['MemoryController.cs','AdminMemoryController.cs']:
                return result('KNOWLEDGE','knowledge.'+{'StoreClaims':'record','GetWriteStatus':'write_status','GetWriteDiagnostics':'diagnostics','GetDiagnostics':'diagnostics','DeleteBySource':'delete_by_source','DeleteTestData':'delete_selected','DeleteByIds':'delete_selected','Search':'search','Subgraph':'subgraph','GetEntityBundle':'entity_bundle','GetEntityGraph':'entity_graph','GetClaimBundle':'claim_bundle','ExpandFrontier':'expand_frontier','RenderSummary':'render_summary','Query':'query','Graph':'subgraph','GetEntity':'read'}[n],disposition='adapt',reason='Unified scoped Knowledge supports fresh data and explicit deletion; legacy memory/knowledge migration is excluded')
            if f=='NodesController.cs':return result('SOURCE','code.'+{'ByFile':'nodes_by_file','Detail':'node_detail','Source':'source','SetDoNotTrust':'set_trust','Search':'search'}[n])
            if f=='RepositoriesController.cs':
                if n in ['Health','Security','Metrics','Hotspots','BatchStatus','ReAnalyze']:return result('ANALYSIS','analysis.'+{'Health':'health','Security':'security','Metrics':'metrics','Hotspots':'hotspots','BatchStatus':'status','ReAnalyze':'reanalyze'}[n])
                if n in ['Impact','FileImpact']:return result('GRAPH','code.impact' if n=='Impact' else 'code.file_impact')
                return result('REPOSITORIES','repositories.'+{'List':'list','Detail':'get','Nodes':'nodes','Delete':'remove_index','Readme':'readme'}[n],reason='Indexed-repository removal is not local primary-source deletion')
            if f=='WorkViewsController.cs':return result('HISTORY',{'Sessions':'sessions.list','Resources':'resources.holders','Messages':'agents.messages','RecentChanges':'history.events','GitHubEvidence':'history.git_links','ItemChanges':'history.events'}[n])
            if f=='WorkImportsController.cs':
                if n=='MemoryCandidates':return result('EXCLUDED',disposition='excluded',reason='IMPORT-01 excludes legacy memory migration')
                if n=='GetAttachment':return result('IMPORT','artifacts.read')
                return result('IMPORT','imports.'+{'GetSettings':'preview','SaveSettings':'configure','StartRun':'apply','ListRuns':'runs','GetRun':'inspect'}[n],disposition='adapt',reason='Import selected existing CodeGraph work/history only; no Shortcut/Kanban live-source importer')
            if f=='WikiController.cs':return result('CONVENTIONS','conventions.'+{'ListSections':'list','GetSectionTree':'list','GetPage':'read','CreatePage':'edit','CreateOrUpload':'import','UpdatePage':'edit','DeletePage':'delete','MovePage':'move','DownloadAttachment':'resources','DeleteAttachment':'delete_resource'}[n],disposition='adapt',reason='Retain versioned guidance/content/resources in conventions/Knowledge; standalone wiki navigation and legacy corpus migration are not selected')
            if f=='AdminMcpHubController.cs':return result('MCP','database.configure_sensitive_columns' if 'SensitiveColumn' in n else 'history.tool_usage' if n=='Audit' else 'mcp.discover' if n in ['Catalog','DiscoverShimProvider'] else 'mcp.configure',disposition='adapt',reason='Native external-server/tool and connection policy replaces remote hub; excluded legacy integrations stay absent')
            table={'AdminLogsController.cs':('DIAGNOSTICS','workspace.logs'),'AdminReportsController.cs':('USAGE','router.usage','history.tool_usage'),'AskController.cs':('SESSIONS','sessions.list','sessions.transcript','sessions.append_message','runs.stop'),'ClientErrorsController.cs':('DIAGNOSTICS','workspace.record_diagnostic'),'ClustersController.cs':('ANALYSIS','analysis.clusters','analysis.cluster_detail'),'DatabaseSourcesController.cs':('DATABASE','schemas.configure','schemas.refresh','schemas.remove_index'),'DreamingEffectsController.cs':('DREAMING','dreaming.report'),'GitHubWebhookController.cs':('GITHUB','github.issue_sync'),'GraphController.cs':('GRAPH','code.overview'),'LlmProvidersController.cs':('ROUTER','router.connections','router.catalog','router.configure'),'ProjectReviewsController.cs':('REVIEWS','reviews.request','reviews.inspect'),'RepositoryReviewsController.cs':('REVIEWS','reviews.request','reviews.inspect'),'SchemasController.cs':('DATABASE','schemas.connections','schemas.objects'),'SearchController.cs':('GRAPH','code.search'),'UserMcpCredentialsController.cs':('MCP','mcp.configure'),'WorkMessageStreamController.cs':('SESSIONS','agents.messages')}
            if f in table:
                g,*ops=table[f];return result(g,*ops,disposition='adapt',reason='Retain application behavior through native operations/streams; GitHub polling replaces inbound webhook hosting where applicable')
        if kind=='ui':
            path=e['name'];g,ops=('NAVIGATION',['ui.current_screen'])
            if path.startswith(('auth/','admin','access-tokens')) or path=='admins':return result('SERVER_REPLACEMENT','settings.read_effective',disposition='replace',reason='Native single-user navigation/permissions replace web sign-in/admin/token pages')
            if path.startswith('wiki') or path.startswith(':section') or path.startswith('conventions'):g,ops='CONVENTIONS',['conventions.list','conventions.read','conventions.edit']
            elif path.startswith(('repos','nodes','search','graph','clusters','impact')):g,ops='NAVIGATION',['repositories.get','code.search','code.source','analysis.clusters']
            elif path.startswith('schemas') or path=='database-sources':g,ops='DATABASE',['schemas.objects','schemas.configure']
            elif path in ['memory','knowledge','lessons','review-queue']:g,ops='KNOWLEDGE',['knowledge.search','knowledge.read','approvals.list']
            elif 'dreaming' in path:g,ops='DREAMING',['dreaming.runs','dreaming.report']
            elif path.startswith('work'):g,ops='WORK',['projects.get','features.get','tasks.get','sessions.list']
            elif path=='ask' or path=='assistant-debug':g,ops='SESSIONS',['sessions.transcript','sessions.append_message']
            elif path in ['reports','llm']:g,ops='USAGE',['router.usage','router.connections']
            elif path=='logs' or path=='db-health':g,ops='DIAGNOSTICS',['workspace.diagnostics','workspace.logs']
            elif path=='schedules':g,ops='JOBS',['jobs.list','jobs.edit']
            elif path=='mcp-hub':g,ops='MCP',['mcp.servers','mcp.configure']
            elif path in ['settings','operations','sections','exclusions','prompts']:g,ops='SETTINGS',['settings.read_effective','settings.save_revision']
            return result(g,*ops,disposition='adapt',reason='Native workspace/view over shared operations replaces Angular route; nesting and layout need native UI acceptance')
        raise ValueError('Unmapped source entry '+e['id'])

    rows=[]
    for e in data['entries']:
        binding=classify(e)
        rows.append({k:v for k,v in e.items() if k not in ['declaration','calls','definition']} | binding)
    assert len({r['id'] for r in rows})==len(rows)
    assert not data['errors']
    result_data={k:v for k,v in data.items() if k!='entries'}
    result_data.update(status='Complete disposition accounting for the declared scan boundary; no runtime parity claim',countsByDisposition=dict(Counter(r['disposition'] for r in rows)),countsByAcceptanceGroup=dict(Counter(r['group'] for r in rows)),entries=rows)
    output.parent.mkdir(parents=True,exist_ok=True);output.write_text(json.dumps(result_data,indent=2)+'\n')
    print(json.dumps({'entries':len(rows),'sourceKinds':data['counts'],'dispositions':result_data['countsByDisposition'],'groups':result_data['countsByAcceptanceGroup']}))


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--source',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args();main(a.source,a.output)
