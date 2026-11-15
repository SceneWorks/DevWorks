-- Whether a graph result may list every dependent: an edge that does not resolve in scope makes
-- an impact result incomplete. An edge that names no repository resolves (or not) inside its own
-- view, so a one-layer view's count of those is recorded once, when the view is stored, instead of
-- resolving every edge of the scope per request. Edges that name a repository resolve against the
-- request's scope and are read through graph_edges_named_repository.
CREATE TABLE graph_view_unresolved (
 view_id TEXT PRIMARY KEY REFERENCES index_views,
 edge_count INTEGER NOT NULL CHECK(edge_count>=0)
) STRICT;
CREATE INDEX graph_edges_named_repository ON graph_edges(generation_id)
    WHERE json_type(payload_json,'$.sourceRepositoryId') IS NOT NULL OR json_type(payload_json,'$.targetRepositoryId') IS NOT NULL;
INSERT INTO graph_view_unresolved(view_id,edge_count) SELECT v.id,(SELECT count(*) FROM source_units u JOIN graph_edges e ON e.generation_id=u.generation_id AND e.unit_key=u.unit_key WHERE u.generation_id=v.baseline_id AND u.action='Replace' AND json_type(e.payload_json,'$.sourceRepositoryId') IS NULL AND json_type(e.payload_json,'$.targetRepositoryId') IS NULL AND (coalesce(json_type(e.payload_json,'$.sourceUnit'),'text')!='text' OR coalesce(json_type(e.payload_json,'$.targetUnit'),'text')!='text' OR (SELECT count(*) FROM graph_nodes n JOIN source_units nu ON nu.generation_id=n.generation_id AND nu.unit_key=n.unit_key WHERE n.generation_id=u.generation_id AND n.node_id=e.source_node_id AND nu.action='Replace' AND (json_type(e.payload_json,'$.sourceUnit') IS NULL OR n.unit_key=json_extract(e.payload_json,'$.sourceUnit')))!=1 OR (SELECT count(*) FROM graph_nodes n JOIN source_units nu ON nu.generation_id=n.generation_id AND nu.unit_key=n.unit_key WHERE n.generation_id=u.generation_id AND n.node_id=e.target_node_id AND nu.action='Replace' AND (json_type(e.payload_json,'$.targetUnit') IS NULL OR n.unit_key=json_extract(e.payload_json,'$.targetUnit')))!=1)) FROM index_views v WHERE v.branch_id IS NULL AND v.worktree_id IS NULL;
