-- Provider connections and SystemOne decision models (ROUTE-09, ROUTE-10, ROUTE-18, SEC-01, DW-18).
-- Additive: two new tables, one nullable column on router_models and a correction of the
-- untouched catalog seed; it references no table of migrations 14-16.
--
-- No secret is ever stored here: a connection names its credential by an opaque SecretStore
-- handle (credential_ref) and a credential revision. The authentication lifecycle stored here
-- is the durable part (Disconnected, Connected, ReauthenticationRequired, Unavailable); a running
-- sign-in or refresh and an unaccepted risk notice are derived in the process, never stored.
-- Migration 1's never-written provider_connections/provider_models placeholders stay untouched
-- (pricing and attempt records reference them); the router keeps its own router_* tables, as
-- migration 13 did for the catalog.
CREATE TABLE router_connections (
    id TEXT PRIMARY KEY CHECK (
        length(id) BETWEEN 1 AND 128 AND id NOT GLOB '*[^A-Za-z0-9_-]*'
    ),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    profile TEXT NOT NULL CHECK (
        profile IN (
            'OpenAiResponses', 'CodexSubscriptionResponses', 'AnthropicMessages',
            'ClaudeSubscriptionMessages', 'OpenRouterChat', 'CompatibleChat', 'Embedding',
            'SystemOne'
        )
    ),
    endpoint TEXT NOT NULL CHECK (
        length(endpoint) BETWEEN 8 AND 4096
        AND (endpoint GLOB 'https://?*' OR endpoint GLOB 'http://?*')
    ),
    -- One handle belongs to one connection at most.
    credential_ref TEXT UNIQUE CHECK (
        credential_ref IS NULL OR (
            length(credential_ref) BETWEEN 1 AND 128
            AND credential_ref NOT GLOB '*[^A-Za-z0-9_-]*'
        )
    ),
    credential_revision INTEGER NOT NULL CHECK (credential_revision >= 0),
    credential_epoch INTEGER CHECK (credential_epoch IS NULL OR credential_epoch >= 0),
    lifecycle TEXT NOT NULL CHECK (
        lifecycle IN ('Disconnected', 'Connected', 'ReauthenticationRequired', 'Unavailable')
    ),
    state_reason TEXT CHECK (state_reason IS NULL OR length(state_reason) BETWEEN 1 AND 4096),
    account_digest TEXT CHECK (account_digest IS NULL OR length(account_digest) = 64),
    access_expires_at INTEGER CHECK (access_expires_at IS NULL OR access_expires_at >= 0),
    -- The subscription risk acceptance and what it is bound to (all set or all NULL).
    risk_notice_sha256 TEXT CHECK (risk_notice_sha256 IS NULL OR length(risk_notice_sha256) = 64),
    risk_binding TEXT CHECK (risk_binding IS NULL OR length(risk_binding) = 64),
    risk_restore_epoch INTEGER CHECK (risk_restore_epoch IS NULL OR risk_restore_epoch >= 0),
    risk_accepted_by TEXT CHECK (risk_accepted_by IS NULL OR length(risk_accepted_by) BETWEEN 1 AND 128),
    risk_accepted_at INTEGER CHECK (risk_accepted_at IS NULL OR risk_accepted_at >= 0),
    discovered_json TEXT CHECK (
        discovered_json IS NULL OR (json_valid(discovered_json) AND json_type(discovered_json) = 'array')
    ),
    discovered_at INTEGER CHECK (discovered_at IS NULL OR discovered_at >= 0),
    healthy INTEGER CHECK (healthy IS NULL OR healthy IN (0, 1)),
    checked_at INTEGER CHECK (checked_at IS NULL OR checked_at >= 0),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    CHECK (
        (risk_notice_sha256 IS NULL) = (risk_binding IS NULL)
        AND (risk_notice_sha256 IS NULL) = (risk_restore_epoch IS NULL)
        AND (risk_notice_sha256 IS NULL) = (risk_accepted_by IS NULL)
        AND (risk_notice_sha256 IS NULL) = (risk_accepted_at IS NULL)
    ),
    -- Only the Claude subscription profile carries a risk acceptance.
    CHECK (risk_notice_sha256 IS NULL OR profile = 'ClaudeSubscriptionMessages'),
    CHECK ((credential_ref IS NULL) = (credential_epoch IS NULL))
) STRICT;

-- SystemOne decision models: their own model IDs, never conversational catalog models or tier
-- members (ROUTE-18). The repository keeps the two ID spaces distinct.
CREATE TABLE router_decision_models (
    id TEXT PRIMARY KEY CHECK (
        length(id) BETWEEN 1 AND 128 AND id NOT GLOB '*[^A-Za-z0-9_-]*'
    ),
    display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 256),
    connection_id TEXT NOT NULL REFERENCES router_connections(id),
    upstream_model_id TEXT NOT NULL CHECK (length(upstream_model_id) BETWEEN 1 AND 4096),
    question_types_json TEXT NOT NULL CHECK (
        json_valid(question_types_json) AND json_type(question_types_json) = 'array'
    ),
    accepts_images INTEGER NOT NULL CHECK (accepts_images IN (0, 1)),
    accepts_videos INTEGER NOT NULL CHECK (accepts_videos IN (0, 1)),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    extension_schemas_json TEXT NOT NULL CHECK (
        json_valid(extension_schemas_json) AND json_type(extension_schemas_json) = 'array'
    ),
    result_schema_json TEXT NOT NULL CHECK (
        json_valid(result_schema_json) AND json_type(result_schema_json) = 'object'
    ),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0)
) STRICT;
CREATE INDEX router_decision_models_connection ON router_decision_models(connection_id);

-- A catalog model may be bound to the one connection that serves it (for example a model of
-- one local OpenAI-compatible endpoint among several). Not a foreign key: removing a bound
-- connection is refused by the repository instead, and a NULL binding lets any connection of
-- the model's endpoint profile serve it, as before.
ALTER TABLE router_models ADD COLUMN connection_id TEXT CHECK (
    connection_id IS NULL OR (
        length(connection_id) BETWEEN 1 AND 128 AND connection_id NOT GLOB '*[^A-Za-z0-9_-]*'
    )
);
CREATE INDEX router_models_connection ON router_models(connection_id) WHERE connection_id IS NOT NULL;

-- Seed correction (DW-18 verification of the DW-17 engineering defaults): the four seeded Claude
-- models accept output_config.effort low, medium, high, xhigh and max on both the subscription
-- and the API profile, so XHigh was missing. Only rows still exactly as seeded (revision 1,
-- never edited) are corrected; an edited row keeps the user's declaration.
UPDATE router_models
SET efforts_json = '["Low","Medium","High","XHigh","Max"]'
WHERE id IN ('fable', 'opus', 'sonnet', 'haiku')
    AND revision = 1
    AND updated_at = 0
    AND efforts_json = '["Low","Medium","High","Max"]';
