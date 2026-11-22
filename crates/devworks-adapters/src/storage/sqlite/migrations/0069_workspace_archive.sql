-- Archive state belongs to the workspace. Directory grants never transfer across restore epochs.
CREATE TABLE workspace_backup_folders (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES actors(id),
    expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms>=0),
    operation_id TEXT UNIQUE REFERENCES operations(id),
    record_json TEXT NOT NULL CHECK(json_valid(record_json))
) STRICT;
CREATE INDEX workspace_backup_folders_expiry ON workspace_backup_folders(expires_at_ms,id);

CREATE TABLE workspace_archive_runs (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES actors(id),
    operation_id TEXT UNIQUE REFERENCES operations(id),
    revision INTEGER NOT NULL CHECK(revision>0),
    phase TEXT NOT NULL CHECK(phase IN
        ('Prepared','Queued','AwaitingRestart','Published','Migrated','Restored','Cancelled','Failed','Unknown')),
    record_json TEXT NOT NULL CHECK(json_valid(record_json))
) STRICT;
CREATE UNIQUE INDEX workspace_archive_one_awaiting_restore
    ON workspace_archive_runs(phase) WHERE phase='AwaitingRestart';

-- Provenance of the replaced and imported stores. The original receipt keeps its earlier
-- scope and operation identity here, separate from a colliding imported operation ID.
CREATE TABLE workspace_restore_history (
    epoch INTEGER PRIMARY KEY CHECK(epoch>0),
    proposal TEXT NOT NULL,
    operation TEXT NOT NULL,
    source_manifest TEXT NOT NULL CHECK(json_valid(source_manifest)),
    prior_manifest TEXT NOT NULL CHECK(json_valid(prior_manifest)),
    receipt TEXT NOT NULL CHECK(json_valid(receipt))
) STRICT;

CREATE TABLE workspace_restore_effects (
    epoch INTEGER NOT NULL REFERENCES workspace_restore_history(epoch),
    timeline TEXT NOT NULL CHECK(timeline IN ('source','destination')),
    origin_store TEXT NOT NULL,
    origin_epoch INTEGER NOT NULL CHECK(origin_epoch>=0),
    operation TEXT NOT NULL,
    capability TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    root TEXT,
    state TEXT NOT NULL,
    unresolved INTEGER NOT NULL CHECK(unresolved IN (0,1)),
    PRIMARY KEY(epoch,timeline,operation)
) STRICT;
CREATE INDEX workspace_restore_effect_scope ON workspace_restore_effects(root,capability) WHERE unresolved=1;
