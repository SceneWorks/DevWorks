-- Unified Knowledge (KNOW-01..08, DW-30): entities, atomic claims with their tags, evidence,
-- verification basis, typed relations, revision history and the write journal; lesson recurrence
-- events, challenges, local addresses and per-session rule delivery; observed source revisions,
-- user import identity and lexical (FTS5) projections. Additive: new tables and indexes only.
-- Migration 0001's placeholder knowledge_entries/knowledge_links/knowledge_revisions/
-- knowledge_tags/knowledge_shares tables were never written by any build and stay unused; these
-- tables are the one authoritative Knowledge store (no second memory, lesson or wiki catalog).
--
-- Scope is a property of every record: `workspace` (general Knowledge, available to every
-- project) or `project:<id>` (that project only). Tags (memory, knowledge, lesson, ...) never
-- change scope. Each scope has a revision that advances with every change of its membership or
-- content, so cursors and previews read under one revision expire when it moves.
CREATE TABLE knowledge_scopes (
    scope_key TEXT PRIMARY KEY CHECK (
        scope_key = 'workspace' OR (scope_key GLOB 'project:?*' AND length(scope_key) <= 136)
    ),
    revision INTEGER NOT NULL CHECK (revision >= 1)
) STRICT;

CREATE TABLE knowledge_entities (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 256),
    entity_type TEXT NOT NULL CHECK (length(entity_type) BETWEEN 1 AND 256),
    aliases_json TEXT NOT NULL CHECK (json_valid(aliases_json) AND json_type(aliases_json) = 'array'),
    -- Case-folded `type|name` (folded in Rust, never by SQLite's ASCII-only lower()).
    canonical_key TEXT NOT NULL CHECK (length(canonical_key) BETWEEN 1 AND 1024),
    canonical_name TEXT CHECK (canonical_name IS NULL OR length(canonical_name) BETWEEN 1 AND 256),
    external_id TEXT CHECK (external_id IS NULL OR length(external_id) BETWEEN 1 AND 4096),
    source TEXT CHECK (source IS NULL OR length(source) BETWEEN 1 AND 256),
    author_id TEXT NOT NULL REFERENCES actors(id),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    deleted_at INTEGER CHECK (deleted_at IS NULL OR deleted_at >= created_at),
    CHECK (
        (scope_key = 'workspace' AND project_id IS NULL)
        OR (project_id IS NOT NULL AND scope_key = 'project:' || project_id)
    )
) STRICT;
CREATE UNIQUE INDEX knowledge_entities_key ON knowledge_entities(scope_key, canonical_key)
    WHERE deleted_at IS NULL;
CREATE INDEX knowledge_entities_source ON knowledge_entities(scope_key, source)
    WHERE source IS NOT NULL;

-- One row per claim (its current revision). Indexed columns serve scoped filters; body_json
-- holds the rest of the authored content and provenance. A lesson carries its rule state.
CREATE TABLE knowledge_claims (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    kind TEXT NOT NULL CHECK (
        kind IN ('Fact', 'Decision', 'Lesson', 'Constraint', 'Finding', 'Proposal')
    ),
    entry_kind TEXT NOT NULL CHECK (
        entry_kind IN (
            'Goal', 'Decision', 'Constraint', 'Fact', 'Gotcha', 'Attempt', 'Architecture',
            'Lesson', 'Finding', 'Proposal'
        )
    ),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('Active', 'Superseded', 'Retracted', 'Deleted')),
    subject_id TEXT NOT NULL REFERENCES knowledge_entities(id),
    predicate TEXT NOT NULL CHECK (length(predicate) BETWEEN 1 AND 256),
    object_id TEXT CHECK (object_id IS NULL OR length(object_id) BETWEEN 1 AND 128),
    source TEXT CHECK (source IS NULL OR length(source) BETWEEN 1 AND 256),
    author_id TEXT NOT NULL REFERENCES actors(id),
    session_id TEXT CHECK (session_id IS NULL OR length(session_id) BETWEEN 1 AND 128),
    rule_state TEXT CHECK (
        rule_state IS NULL
        OR rule_state IN ('Candidate', 'Proposed', 'Effective', 'Rejected', 'Retired')
    ),
    lesson_source TEXT CHECK (
        lesson_source IS NULL OR lesson_source IN (
            'Prompt', 'Review', 'RelayedPrompt', 'RelayedReview', 'Agent', 'Subagent', 'BotReview'
        )
    ),
    applicability_json TEXT CHECK (
        applicability_json IS NULL
        OR (json_valid(applicability_json) AND json_type(applicability_json) = 'object')
    ),
    effective_at INTEGER CHECK (effective_at IS NULL OR effective_at >= 0),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    deleted_at INTEGER CHECK (deleted_at IS NULL OR deleted_at >= created_at),
    body_json TEXT NOT NULL CHECK (json_valid(body_json) AND json_type(body_json) = 'object'),
    CHECK (
        (scope_key = 'workspace' AND project_id IS NULL)
        OR (project_id IS NOT NULL AND scope_key = 'project:' || project_id)
    ),
    CHECK ((kind = 'Lesson') = (rule_state IS NOT NULL AND lesson_source IS NOT NULL)),
    CHECK ((lifecycle = 'Deleted') = (deleted_at IS NOT NULL))
) STRICT;
CREATE INDEX knowledge_claims_recent
    ON knowledge_claims(scope_key, lifecycle, updated_at DESC, id DESC);
CREATE INDEX knowledge_claims_subject ON knowledge_claims(subject_id, predicate);
CREATE INDEX knowledge_claims_object ON knowledge_claims(object_id) WHERE object_id IS NOT NULL;
CREATE INDEX knowledge_claims_source ON knowledge_claims(scope_key, source)
    WHERE source IS NOT NULL;
CREATE INDEX knowledge_claims_lessons
    ON knowledge_claims(scope_key, rule_state, updated_at DESC, id DESC) WHERE kind = 'Lesson';

CREATE TABLE knowledge_claim_tags (
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    tag TEXT NOT NULL CHECK (length(tag) BETWEEN 1 AND 256),
    PRIMARY KEY (claim_id, tag)
) STRICT, WITHOUT ROWID;
CREATE INDEX knowledge_claim_tags_lookup ON knowledge_claim_tags(tag, claim_id);

CREATE TABLE knowledge_evidence (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    kind TEXT NOT NULL CHECK (
        kind IN ('Conversation', 'Document', 'Code', 'Observation', 'External')
    ),
    source_ref TEXT NOT NULL CHECK (length(source_ref) BETWEEN 1 AND 4096),
    artifact_json TEXT CHECK (
        artifact_json IS NULL OR (json_valid(artifact_json) AND json_type(artifact_json) = 'object')
    ),
    locator TEXT CHECK (locator IS NULL OR length(locator) BETWEEN 1 AND 4096),
    snippet TEXT NOT NULL,
    metadata_json TEXT CHECK (metadata_json IS NULL OR json_valid(metadata_json)),
    source_revision TEXT CHECK (
        source_revision IS NULL OR length(source_revision) BETWEEN 1 AND 4096
    ),
    author_id TEXT NOT NULL REFERENCES actors(id),
    recorded_at INTEGER NOT NULL CHECK (recorded_at >= 0),
    deleted_at INTEGER CHECK (deleted_at IS NULL OR deleted_at >= recorded_at),
    CHECK (
        (scope_key = 'workspace' AND project_id IS NULL)
        OR (project_id IS NOT NULL AND scope_key = 'project:' || project_id)
    )
) STRICT;
CREATE TABLE knowledge_claim_evidence (
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    evidence_id TEXT NOT NULL REFERENCES knowledge_evidence(id),
    PRIMARY KEY (claim_id, evidence_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX knowledge_evidence_users ON knowledge_claim_evidence(evidence_id, claim_id);

-- The source revisions a claim was recorded or verified against. A later observed revision of
-- one of those sources (knowledge_source_heads) makes the claim read stale until re-verified.
CREATE TABLE knowledge_claim_basis (
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    source_ref TEXT NOT NULL CHECK (length(source_ref) BETWEEN 1 AND 4096),
    source_revision TEXT NOT NULL CHECK (length(source_revision) BETWEEN 1 AND 4096),
    PRIMARY KEY (claim_id, source_ref)
) STRICT, WITHOUT ROWID;
CREATE INDEX knowledge_basis_source ON knowledge_claim_basis(source_ref, claim_id);
CREATE TABLE knowledge_source_heads (
    source_ref TEXT PRIMARY KEY CHECK (length(source_ref) BETWEEN 1 AND 4096),
    source_revision TEXT NOT NULL CHECK (length(source_revision) BETWEEN 1 AND 4096),
    observed_at INTEGER NOT NULL CHECK (observed_at >= 0)
) STRICT;

CREATE TABLE knowledge_relations (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    subject_id TEXT NOT NULL CHECK (length(subject_id) BETWEEN 1 AND 128),
    predicate TEXT NOT NULL CHECK (length(predicate) BETWEEN 1 AND 256),
    object_id TEXT NOT NULL CHECK (length(object_id) BETWEEN 1 AND 128),
    evidence_json TEXT NOT NULL CHECK (
        json_valid(evidence_json) AND json_type(evidence_json) = 'array'
    ),
    author_id TEXT NOT NULL REFERENCES actors(id),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    removed_at INTEGER CHECK (removed_at IS NULL OR removed_at >= created_at),
    write_id TEXT NOT NULL CHECK (length(write_id) BETWEEN 1 AND 128),
    -- A same_as lesson merge: the citations it added to the object (an unmerge takes back
    -- exactly these, never evidence the object already cited).
    added_evidence_json TEXT NOT NULL DEFAULT '[]' CHECK (
        json_valid(added_evidence_json) AND json_type(added_evidence_json) = 'array'
    ),
    CHECK (
        (scope_key = 'workspace' AND project_id IS NULL)
        OR (project_id IS NOT NULL AND scope_key = 'project:' || project_id)
    )
) STRICT;
-- One live relation per scope: another project's identical relation is its own record.
CREATE UNIQUE INDEX knowledge_relations_live
    ON knowledge_relations(scope_key, subject_id, predicate, object_id) WHERE removed_at IS NULL;
CREATE INDEX knowledge_relations_object ON knowledge_relations(object_id);

-- Revision history: one immutable snapshot per revision of an entity or claim, attributed and
-- tied to the write that made it (the action journal a guarded revert reads, DREAM-04).
CREATE TABLE knowledge_history (
    target_id TEXT NOT NULL CHECK (length(target_id) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('Entity', 'Claim')),
    change TEXT NOT NULL CHECK (length(change) BETWEEN 1 AND 64),
    author_id TEXT NOT NULL REFERENCES actors(id),
    at INTEGER NOT NULL CHECK (at >= 0),
    write_id TEXT NOT NULL CHECK (length(write_id) BETWEEN 1 AND 128),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('Active', 'Superseded', 'Retracted', 'Deleted')),
    snapshot_json TEXT NOT NULL CHECK (
        json_valid(snapshot_json) AND json_type(snapshot_json) = 'object'
    ),
    PRIMARY KEY (target_id, revision)
) STRICT, WITHOUT ROWID;
CREATE INDEX knowledge_history_write ON knowledge_history(write_id);

-- The durable receipt of every committed Knowledge write (knowledge.write_status) with its
-- before/after revisions per target.
CREATE TABLE knowledge_writes (
    write_id TEXT PRIMARY KEY CHECK (length(write_id) BETWEEN 1 AND 128),
    operation TEXT NOT NULL CHECK (length(operation) BETWEEN 1 AND 64),
    scope_key TEXT NOT NULL,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    session_id TEXT,
    at INTEGER NOT NULL CHECK (at >= 0),
    scope_revision INTEGER NOT NULL CHECK (scope_revision >= 1),
    entity_ids_json TEXT NOT NULL CHECK (json_valid(entity_ids_json)),
    claim_ids_json TEXT NOT NULL CHECK (json_valid(claim_ids_json)),
    changes_json TEXT NOT NULL CHECK (json_valid(changes_json)),
    diagnostics_json TEXT NOT NULL CHECK (json_valid(diagnostics_json))
) STRICT;
CREATE INDEX knowledge_writes_recent ON knowledge_writes(scope_key, at DESC, write_id DESC);

-- Distinct source events a lesson recurred in (a repeat of one event is one recurrence).
CREATE TABLE knowledge_lesson_events (
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    event_key TEXT NOT NULL CHECK (length(event_key) BETWEEN 1 AND 4200),
    source TEXT NOT NULL CHECK (
        source IN (
            'Prompt', 'Review', 'RelayedPrompt', 'RelayedReview', 'Agent', 'Subagent', 'BotReview'
        )
    ),
    source_ref TEXT,
    session_id TEXT,
    actor_id TEXT NOT NULL REFERENCES actors(id),
    at INTEGER NOT NULL CHECK (at >= 0),
    write_id TEXT NOT NULL CHECK (length(write_id) BETWEEN 1 AND 128),
    -- The lessons this event was merged from, oldest first: a same_as merge pushes the lesson
    -- it leaves and an unmerge pops it, so nested merges stay reversible.
    merge_path TEXT NOT NULL DEFAULT '[]' CHECK (
        json_valid(merge_path) AND json_type(merge_path) = 'array'
    ),
    PRIMARY KEY (claim_id, event_key)
) STRICT, WITHOUT ROWID;
CREATE INDEX knowledge_lesson_events_merged
    ON knowledge_lesson_events(json_extract(merge_path, '$[#-1]'))
    WHERE merge_path <> '[]';

CREATE TABLE knowledge_challenges (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    state TEXT NOT NULL CHECK (state IN ('Open', 'Upheld', 'Retired')),
    reason TEXT NOT NULL,
    evidence_json TEXT NOT NULL CHECK (
        json_valid(evidence_json) AND json_type(evidence_json) = 'array'
    ),
    author_id TEXT NOT NULL REFERENCES actors(id),
    opened_at INTEGER NOT NULL CHECK (opened_at >= 0),
    resolved_at INTEGER CHECK (resolved_at IS NULL OR resolved_at >= opened_at),
    resolved_by TEXT REFERENCES actors(id),
    resolution TEXT,
    CHECK ((state = 'Open') = (resolved_at IS NULL AND resolved_by IS NULL))
) STRICT;
CREATE INDEX knowledge_challenges_claim ON knowledge_challenges(claim_id, opened_at, id);

-- Where a lesson is addressed outside Knowledge (code, instructions, a skill, documentation or
-- a test, KNOW-06). The same kind and name with a new reference updates the link.
CREATE TABLE knowledge_addresses (
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    kind TEXT NOT NULL CHECK (kind IN ('Code', 'Instruction', 'Skill', 'Documentation', 'Test')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    reference TEXT CHECK (reference IS NULL OR length(reference) BETWEEN 1 AND 4096),
    actor_id TEXT NOT NULL REFERENCES actors(id),
    at INTEGER NOT NULL CHECK (at >= 0),
    PRIMARY KEY (claim_id, kind, name)
) STRICT, WITHOUT ROWID;

-- Which rule revisions were supplied to which session (KNOW-06).
CREATE TABLE knowledge_deliveries (
    session_id TEXT NOT NULL REFERENCES sessions(id),
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    delivered_at INTEGER NOT NULL CHECK (delivered_at >= 0),
    PRIMARY KEY (session_id, claim_id, revision)
) STRICT, WITHOUT ROWID;
CREATE INDEX knowledge_deliveries_claim ON knowledge_deliveries(claim_id, delivered_at);

-- User data import identity: one claim per imported candidate of an immutable artifact version.
CREATE TABLE knowledge_import_map (
    artifact_version_id TEXT NOT NULL CHECK (length(artifact_version_id) BETWEEN 1 AND 128),
    candidate INTEGER NOT NULL CHECK (candidate >= 0),
    claim_id TEXT NOT NULL REFERENCES knowledge_claims(id),
    write_id TEXT NOT NULL CHECK (length(write_id) BETWEEN 1 AND 128),
    PRIMARY KEY (artifact_version_id, candidate)
) STRICT, WITHOUT ROWID;

-- Lexical projections. unicode61 folds case over Unicode (not only ASCII) and diacritics, the
-- same way for indexed text and query words. Deleted records have no row.
CREATE VIRTUAL TABLE knowledge_claim_fts USING fts5(
    claim_id UNINDEXED, text, tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE knowledge_entity_fts USING fts5(
    entity_id UNINDEXED, text, tokenize = 'unicode61 remove_diacritics 2'
);
