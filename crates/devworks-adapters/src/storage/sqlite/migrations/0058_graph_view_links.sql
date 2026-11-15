-- The edges an overlay view links differently from the layer that stores them (an inherited
-- edge whose target rebinds against the composed view), so an endpoint lookup reads the
-- composed link through an index instead of relinking every edge in scope. A view's link set
-- row records that its links were computed, also when it has none; an overlay view without one
-- (every overlay view this migration finds) is linked by index housekeeping.
CREATE TABLE graph_view_links (
 view_id TEXT NOT NULL REFERENCES index_views,
 generation_id TEXT NOT NULL,
 unit_key TEXT NOT NULL, edge_id TEXT NOT NULL,
 source_node_id TEXT NOT NULL, target_node_id TEXT NOT NULL,
 payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
 PRIMARY KEY(view_id,unit_key,edge_id)
) STRICT;
CREATE INDEX graph_view_links_source ON graph_view_links(view_id,source_node_id);
CREATE INDEX graph_view_links_target ON graph_view_links(view_id,target_node_id);
CREATE TABLE graph_view_link_sets (
 view_id TEXT PRIMARY KEY REFERENCES index_views,
 link_count INTEGER NOT NULL CHECK(link_count>=0)
) STRICT;
