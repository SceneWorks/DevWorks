-- Managed executable package registry (ADDON-01..04, EXT-01..07): the revisioned registry head
-- (the ActivePackageSet revision), one record per package (desired enablement, the active
-- pointer, the rollback predecessor, the pending version, the state generation, configuration,
-- publisher trust and the last failure), immutable verified versions, exact-version approval
-- proposals and retained package state generations.
-- Additive: seven new tables and their indexes. They reference no table of migrations 11-12; the
-- live handler state of the current generation stays in package_handler_state (migration 1).
CREATE TABLE package_registry (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 1)
) STRICT;
INSERT INTO package_registry(singleton, revision) VALUES (1, 1);
CREATE TABLE packages (
    package_id TEXT PRIMARY KEY CHECK (
        length(package_id) BETWEEN 1 AND 128 AND package_id NOT GLOB '*[^A-Za-z0-9_-]*'
    ),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    installed INTEGER NOT NULL CHECK (installed IN (0, 1)),
    active_digest TEXT CHECK (active_digest IS NULL OR length(active_digest) = 64),
    predecessor_digest TEXT CHECK (predecessor_digest IS NULL OR length(predecessor_digest) = 64),
    pending_digest TEXT CHECK (pending_digest IS NULL OR length(pending_digest) = 64),
    state_generation INTEGER NOT NULL CHECK (state_generation >= 0),
    config_json TEXT CHECK (
        config_json IS NULL OR (json_valid(config_json) AND json_type(config_json) = 'object')
    ),
    config_revision INTEGER NOT NULL CHECK (config_revision >= 0),
    trust_json TEXT CHECK (
        trust_json IS NULL OR (json_valid(trust_json) AND json_type(trust_json) = 'object')
    ),
    failure_json TEXT CHECK (
        failure_json IS NULL OR (json_valid(failure_json) AND json_type(failure_json) = 'object')
    ),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    -- An uninstalled package has no active pointer and is never enabled.
    CHECK (installed = 1 OR (enabled = 0 AND active_digest IS NULL))
) STRICT;
-- Identity (package, manifest digest) never changes; only phase, waits and approval move.
CREATE TABLE package_versions (
    package_id TEXT NOT NULL REFERENCES packages(package_id),
    manifest_digest TEXT NOT NULL CHECK (length(manifest_digest) = 64),
    bundle_digest TEXT NOT NULL CHECK (length(bundle_digest) = 64),
    version TEXT NOT NULL CHECK (length(version) BETWEEN 1 AND 128),
    publisher_id TEXT NOT NULL CHECK (length(publisher_id) BETWEEN 1 AND 128),
    signer_key TEXT CHECK (signer_key IS NULL OR length(signer_key) = 64),
    inspection_json TEXT NOT NULL CHECK (
        json_valid(inspection_json) AND json_type(inspection_json) = 'object'
    ),
    provenance_json TEXT NOT NULL CHECK (
        json_valid(provenance_json) AND json_type(provenance_json) = 'object'
    ),
    phase TEXT NOT NULL CHECK (phase IN ('Staged', 'Installed', 'Removed', 'Rejected')),
    waits_json TEXT NOT NULL DEFAULT '[]' CHECK (
        json_valid(waits_json) AND json_type(waits_json) = 'array'
    ),
    approval_json TEXT CHECK (
        approval_json IS NULL OR (json_valid(approval_json) AND json_type(approval_json) = 'object')
    ),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    PRIMARY KEY (package_id, manifest_digest)
) STRICT;
CREATE TABLE package_proposals (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    package_id TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    bundle_digest TEXT NOT NULL CHECK (length(bundle_digest) = 64),
    version TEXT NOT NULL CHECK (length(version) BETWEEN 1 AND 128),
    state TEXT NOT NULL CHECK (state IN ('Pending', 'Approved', 'Rejected', 'Superseded', 'Expired')),
    base_package_id TEXT CHECK (base_package_id IS NULL OR length(base_package_id) BETWEEN 1 AND 128),
    originator TEXT NOT NULL CHECK (length(originator) BETWEEN 1 AND 128),
    origin_json TEXT NOT NULL CHECK (json_valid(origin_json) AND json_type(origin_json) = 'object'),
    summary_json TEXT NOT NULL CHECK (json_valid(summary_json) AND json_type(summary_json) = 'object'),
    decided_by TEXT CHECK (decided_by IS NULL OR length(decided_by) BETWEEN 1 AND 128),
    decided_at INTEGER CHECK (decided_at IS NULL OR decided_at >= created_at),
    decision_json TEXT CHECK (
        decision_json IS NULL OR (json_valid(decision_json) AND json_type(decision_json) = 'object')
    ),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    -- A decided proposal records when; a pending one has no decision yet.
    CHECK ((state = 'Pending') = (decided_at IS NULL)),
    FOREIGN KEY (package_id, manifest_digest) REFERENCES package_versions(package_id, manifest_digest)
) STRICT;
CREATE INDEX package_proposals_contents ON package_proposals(package_id, manifest_digest, created_at);
-- One row per published state generation. The live state of the current generation is the
-- package_handler_state row; when a newer generation is published the retiring generation's
-- final state is retained here (retained_*), so a guarded rollback can restore it.
CREATE TABLE package_state_generations (
    package_id TEXT NOT NULL REFERENCES packages(package_id),
    generation INTEGER NOT NULL CHECK (generation >= 1),
    manifest_digest TEXT NOT NULL CHECK (length(manifest_digest) = 64),
    schema_digest TEXT NOT NULL CHECK (length(schema_digest) = 64),
    seeded_from INTEGER CHECK (seeded_from IS NULL OR (seeded_from >= 1 AND seeded_from < generation)),
    seed_revision INTEGER NOT NULL CHECK (seed_revision >= 0),
    retained_revision INTEGER CHECK (retained_revision IS NULL OR retained_revision >= 0),
    retained_state_json TEXT CHECK (retained_state_json IS NULL OR json_valid(retained_state_json)),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (package_id, generation),
    FOREIGN KEY (package_id, manifest_digest) REFERENCES package_versions(package_id, manifest_digest)
) STRICT;
-- The last fetch of each configured signed publisher index source (available packs).
CREATE TABLE package_indexes (
    source_key TEXT PRIMARY KEY CHECK (length(source_key) = 64),
    location TEXT NOT NULL CHECK (length(location) BETWEEN 1 AND 2048),
    public_key TEXT NOT NULL CHECK (length(public_key) = 64),
    status TEXT NOT NULL CHECK (status IN ('Verified', 'Failed')),
    index_json TEXT CHECK (index_json IS NULL OR (json_valid(index_json) AND json_type(index_json) = 'object')),
    error TEXT CHECK (error IS NULL OR length(error) <= 4096),
    fetched_at INTEGER NOT NULL CHECK (fetched_at >= 0)
) STRICT;
-- The effective availability and active version last announced for each package, so every
-- change is published exactly once (PackageAvailabilityChanged).
CREATE TABLE package_observations (
    package_id TEXT PRIMARY KEY CHECK (length(package_id) BETWEEN 1 AND 128),
    availability TEXT NOT NULL CHECK (
        availability IN ('NotInstalled', 'Disabled', 'MissingDependency', 'Incompatible', 'Ready', 'Failed')
    ),
    version TEXT CHECK (version IS NULL OR length(version) BETWEEN 1 AND 128),
    digest TEXT CHECK (digest IS NULL OR length(digest) = 64),
    revision INTEGER NOT NULL CHECK (revision >= 1)
) STRICT;
