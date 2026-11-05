-- Owned browser sessions (DW-047, UIQA-01..04): the durable lifecycle of every browser the host
-- launched through the process owner, and the index of the evidence each session published.
-- Additive: two new tables and their indexes. Live CDP state (targets' frame/document
-- revisions, snapshots and node maps, buffered notifications) is never stored: it is invalidated
-- by the browser process's exit.
CREATE TABLE browser_sessions (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    boot_id TEXT NOT NULL REFERENCES app_boots,
    state TEXT NOT NULL CHECK (
        state IN ('Starting', 'Ready', 'Stopping', 'Closed', 'Failed', 'Uncertain')
    ),
    owner_id TEXT NOT NULL REFERENCES actors,
    owner_kind TEXT NOT NULL CHECK (
        owner_kind IN ('Human', 'Agent', 'Package', 'McpClient', 'Schedule', 'Host')
    ),
    agent_session_id TEXT,
    project_id TEXT REFERENCES projects,
    root_id TEXT,
    assignment_id TEXT,
    asset_id TEXT NOT NULL CHECK (length(asset_id) BETWEEN 1 AND 64),
    process_id TEXT REFERENCES owned_processes,
    create_operation_id TEXT NOT NULL REFERENCES operations,
    uncertain_operation_id TEXT REFERENCES operations,
    -- The session's latest admitted navigation, input or upload: the next one runs after it.
    last_action_operation_id TEXT REFERENCES operations,
    -- The latest accepted Close; never withdrawn. A session starting when its Close is
    -- accepted keeps Starting with this request until its launch settles.
    close_operation_id TEXT REFERENCES operations,
    targets TEXT NOT NULL CHECK (json_valid(targets) AND json_type(targets) = 'array'),
    detail TEXT CHECK (detail IS NULL OR length(detail) <= 1024),
    observation_seq INTEGER NOT NULL CHECK (observation_seq >= 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    -- A running or stopping browser has a launched tree.
    CHECK (state NOT IN ('Ready', 'Stopping') OR process_id IS NOT NULL),
    -- A session whose close was requested never becomes ready.
    CHECK (state <> 'Ready' OR close_operation_id IS NULL)
) STRICT;
CREATE INDEX browser_session_scope ON browser_sessions(project_id, id);
CREATE INDEX browser_session_unsettled ON browser_sessions(id)
    WHERE state NOT IN ('Closed', 'Failed');

-- One row per published observation: the evidence artifact version of a console, network or
-- action observation. Sequences are allocated by the session row in the same transaction.
CREATE TABLE browser_observations (
    session_id TEXT NOT NULL REFERENCES browser_sessions,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    channel TEXT NOT NULL CHECK (channel IN ('Console', 'Network', 'Action')),
    artifact_id TEXT NOT NULL REFERENCES artifacts,
    version_id TEXT NOT NULL REFERENCES artifact_versions,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1)),
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, sequence)
) STRICT, WITHOUT ROWID;
CREATE INDEX browser_observation_channel ON browser_observations(session_id, channel, sequence);
