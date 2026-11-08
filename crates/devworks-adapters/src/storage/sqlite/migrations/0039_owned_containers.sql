-- Durable ownership intent and lifecycle for optional external container backends.
-- The app never deletes containers, images or volumes through this record.
CREATE TABLE owned_containers (
    id TEXT PRIMARY KEY,
    engine TEXT NOT NULL CHECK(engine IN ('docker','podman','apple')),
    state TEXT NOT NULL CHECK(state IN ('Creating','Created','Starting','Running','Stopping','Stopped','Failed','Uncertain')),
    image TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    owner_kind TEXT NOT NULL,
    session_id TEXT,
    project_id TEXT NOT NULL,
    root_id TEXT,
    assignment_id TEXT,
    create_operation_id TEXT NOT NULL UNIQUE,
    last_operation_id TEXT NOT NULL,
    resource_lease_ids TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(resource_lease_ids)),
    detail TEXT,
    revision INTEGER NOT NULL CHECK(revision > 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX owned_containers_agent_work ON owned_containers(owner_kind,state,id)
    WHERE owner_kind='Agent' AND state IN ('Creating','Starting','Running','Stopping','Uncertain');
CREATE INDEX owned_containers_project ON owned_containers(project_id,id);
