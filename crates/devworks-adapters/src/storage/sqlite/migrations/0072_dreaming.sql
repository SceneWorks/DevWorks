-- Dreaming owns each scope's completed intake and the immutable evidence for each attempt.
CREATE TABLE dreaming_scope_state (
    scope_key TEXT PRIMARY KEY,
    project_id TEXT REFERENCES projects(id),
    settings_revision INTEGER NOT NULL CHECK(settings_revision > 0),
    settings_json TEXT NOT NULL CHECK(json_valid(settings_json) AND json_type(settings_json)='object'),
    completed_cutoffs_json TEXT CHECK(completed_cutoffs_json IS NULL OR
        (json_valid(completed_cutoffs_json) AND json_type(completed_cutoffs_json)='object')),
    completed_at INTEGER,
    CHECK((completed_cutoffs_json IS NULL)=(completed_at IS NULL)),
    CHECK((scope_key='workspace' AND project_id IS NULL) OR
        (project_id IS NOT NULL AND scope_key='project:' || project_id))
) STRICT;

CREATE TABLE dreaming_instruction_revisions (
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    editor_id TEXT NOT NULL REFERENCES actors(id),
    created_at INTEGER NOT NULL,
    artifact_version_id TEXT NOT NULL REFERENCES artifact_versions(id),
    artifact_json TEXT NOT NULL CHECK(json_valid(artifact_json) AND json_type(artifact_json)='object'),
    PRIMARY KEY(scope_key, revision),
    CHECK((scope_key='workspace' AND project_id IS NULL) OR
        (project_id IS NOT NULL AND scope_key='project:' || project_id))
) STRICT, WITHOUT ROWID;

CREATE TABLE dreaming_runs (
    id TEXT PRIMARY KEY,
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    continued_from TEXT REFERENCES dreaming_runs(id),
    operation_id TEXT REFERENCES operations(id),
    initiator_id TEXT NOT NULL REFERENCES actors(id),
    trigger TEXT NOT NULL CHECK(trigger IN ('Manual','Scheduled','Resume')),
    revision INTEGER NOT NULL CHECK(revision > 0),
    generation INTEGER NOT NULL CHECK(generation > 0),
    state TEXT NOT NULL CHECK(state IN
        ('Queued','Running','Disabled','Busy','TooSoon','NoSignal','Blocked','BudgetExhausted',
         'Partial','Failed','Cancelled','OutcomeUnknown','ScheduledFanout','Completed')),
    settings_revision INTEGER NOT NULL CHECK(settings_revision > 0),
    settings_json TEXT NOT NULL CHECK(json_valid(settings_json) AND json_type(settings_json)='object'),
    cutoffs_json TEXT NOT NULL CHECK(json_valid(cutoffs_json) AND json_type(cutoffs_json)='object'),
    started_at INTEGER NOT NULL,
    ended_at INTEGER CHECK(ended_at IS NULL OR ended_at >= started_at),
    checkpoint_json TEXT CHECK(checkpoint_json IS NULL OR
        (json_valid(checkpoint_json) AND json_type(checkpoint_json)='object')),
    report_json TEXT CHECK(report_json IS NULL OR
        (json_valid(report_json) AND json_type(report_json)='object')),
    CHECK((state IN ('Queued','Running'))=(ended_at IS NULL)),
    CHECK((scope_key='workspace' AND project_id IS NULL) OR
        (project_id IS NOT NULL AND scope_key='project:' || project_id))
) STRICT;
CREATE UNIQUE INDEX dreaming_live_scope ON dreaming_runs(scope_key)
    WHERE state IN ('Queued','Running','OutcomeUnknown');
CREATE UNIQUE INDEX dreaming_run_operation ON dreaming_runs(operation_id) WHERE operation_id IS NOT NULL;
CREATE INDEX dreaming_run_page ON dreaming_runs(scope_key, started_at DESC, id DESC);
CREATE INDEX dreaming_run_parent ON dreaming_runs(continued_from) WHERE continued_from IS NOT NULL;
CREATE TRIGGER dreaming_complete_scope AFTER UPDATE OF state ON dreaming_runs
WHEN NEW.state='Completed' AND OLD.state<>'Completed'
BEGIN
    UPDATE dreaming_scope_state
       SET completed_cutoffs_json=NEW.cutoffs_json, completed_at=NEW.ended_at
     WHERE scope_key=NEW.scope_key;
END;

CREATE TABLE dreaming_actions (
    run_id TEXT NOT NULL REFERENCES dreaming_runs(id),
    action_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK(position > 0),
    committed_at INTEGER NOT NULL,
    action_json TEXT NOT NULL CHECK(json_valid(action_json) AND json_type(action_json)='object'),
    PRIMARY KEY(run_id, action_id),
    UNIQUE(run_id, position)
) STRICT, WITHOUT ROWID;

-- Knowledge and reviews already have monotonic event identities. Source changes are recorded
-- independently because a replacement may matter even when the affected claim is already stale.
CREATE INDEX dreaming_knowledge_events ON events(project_id,seq) WHERE kind='KnowledgeWritten';
CREATE INDEX dreaming_review_events ON events(project_id,seq) WHERE kind='PlanReviewSettled';

CREATE TABLE dreaming_source_changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_key TEXT NOT NULL,
    project_id TEXT REFERENCES projects(id),
    source_ref TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    CHECK((scope_key='workspace' AND project_id IS NULL) OR
        (project_id IS NOT NULL AND scope_key='project:' || project_id))
) STRICT;
CREATE INDEX dreaming_source_scope ON dreaming_source_changes(scope_key,seq);

-- Only current sources of live claims seed the first sweep after an upgrade.
INSERT INTO dreaming_source_changes(scope_key,project_id,source_ref,source_revision,observed_at)
    SELECT DISTINCT c.scope_key,c.project_id,h.source_ref,h.source_revision,h.observed_at
      FROM knowledge_source_heads h JOIN knowledge_claim_basis b ON b.source_ref=h.source_ref
      JOIN knowledge_claims c ON c.id=b.claim_id
     WHERE c.lifecycle='Active' AND b.source_revision<>h.source_revision
     ORDER BY h.observed_at,h.source_ref,c.scope_key;

CREATE TRIGGER dreaming_source_observed AFTER INSERT ON knowledge_source_heads BEGIN
    INSERT INTO dreaming_source_changes(scope_key,project_id,source_ref,source_revision,observed_at)
        SELECT DISTINCT c.scope_key,c.project_id,NEW.source_ref,NEW.source_revision,NEW.observed_at
          FROM knowledge_claim_basis b JOIN knowledge_claims c ON c.id=b.claim_id
         WHERE b.source_ref=NEW.source_ref AND c.lifecycle='Active';
END;
CREATE TRIGGER dreaming_source_replaced AFTER UPDATE OF source_revision ON knowledge_source_heads
WHEN NEW.source_revision<>OLD.source_revision BEGIN
    INSERT INTO dreaming_source_changes(scope_key,project_id,source_ref,source_revision,observed_at)
        SELECT DISTINCT c.scope_key,c.project_id,NEW.source_ref,NEW.source_revision,NEW.observed_at
          FROM knowledge_claim_basis b JOIN knowledge_claims c ON c.id=b.claim_id
         WHERE b.source_ref=NEW.source_ref AND c.lifecycle='Active';
END;

CREATE TABLE dreaming_reversions (
    run_id TEXT NOT NULL,
    action_id TEXT NOT NULL,
    reverted_by TEXT NOT NULL REFERENCES actors(id),
    reverted_at INTEGER NOT NULL,
    receipt_json TEXT NOT NULL CHECK(json_valid(receipt_json) AND json_type(receipt_json)='object'),
    PRIMARY KEY(run_id,action_id),
    FOREIGN KEY(run_id,action_id) REFERENCES dreaming_actions(run_id,action_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX dreaming_claim_scope ON knowledge_claims(scope_key,id);
