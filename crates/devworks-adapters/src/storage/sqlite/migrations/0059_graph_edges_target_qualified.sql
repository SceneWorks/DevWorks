-- An edge into another repository names its target by qualified name (`targetQualifiedName`),
-- which the graph reader matches instead of the stored target node ID. Looking up the edges
-- into one node reads those through this index instead of scanning every edge in scope. Only
-- such edges carry the name, so the partial index holds no other row.
CREATE INDEX graph_edges_target_qualified
    ON graph_edges(generation_id,json_extract(payload_json,'$.targetQualifiedName'))
    WHERE json_extract(payload_json,'$.targetQualifiedName') IS NOT NULL;
