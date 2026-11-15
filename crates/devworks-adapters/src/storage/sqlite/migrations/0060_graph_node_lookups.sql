-- A node looked up by its ID alone (an edge endpoint without a unit, a node selected by ID) is
-- read from each selected layer through this index and kept when its unit is visible there,
-- instead of ranking every unit row of the scope.
CREATE INDEX graph_nodes_by_id ON graph_nodes(generation_id,node_id);
-- A .NET project owns the nodes under its directory. A node's owner is read from the projects in
-- the directories on its own path, by the directory of each project's unit (the unit key through
-- its last `/`), instead of every project in scope. Only project nodes are indexed.
CREATE INDEX graph_nodes_project_directory
    ON graph_nodes(generation_id,rtrim(unit_key,replace(unit_key,'/','')))
    WHERE kind='DotnetProject';
