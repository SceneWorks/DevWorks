-- Inference router records (ROUTE-14, ROUTE-15, ROUTE-19, DW-17): the conversational model
-- catalog, persisted round-robin tier cursors, session route pins, router-reported cooldowns and
-- logical request bindings. Additive: five new tables and their indexes; it references no table
-- of migrations 11-12.
--
-- The catalog is editable configuration, seeded once here and kept as edited across restarts.
-- The seed mirrors the retained simple-router catalog (revision a556b353): the default tier
-- members (Max = Fable, Astra; High = Opus, Sol; Mid = Sonnet, Terra; Low = Haiku, Luna, whose
-- membership lives in the routing.tier*.members settings) and Open Router Auto. Declared efforts
-- and capabilities are engineering defaults, editable per model; context windows are unknown
-- (NULL) until declared or discovered, so a context requirement is never assumed to fit.
CREATE TABLE router_models (
    id TEXT PRIMARY KEY CHECK (
        length(id) BETWEEN 1 AND 128 AND id NOT GLOB '*[^A-Za-z0-9_-]*'
    ),
    display_name TEXT NOT NULL CHECK (length(display_name) BETWEEN 1 AND 256),
    -- SystemOne decision models are never conversational catalog models (ROUTE-18).
    endpoint_profile TEXT NOT NULL CHECK (
        endpoint_profile IN (
            'OpenAiResponses', 'CodexSubscriptionResponses', 'AnthropicMessages',
            'ClaudeSubscriptionMessages', 'OpenRouterChat', 'CompatibleChat', 'Embedding'
        )
    ),
    upstream_model_id TEXT NOT NULL CHECK (length(upstream_model_id) BETWEEN 1 AND 4096),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    -- Declared explicit efforts (never 'Default', which delegates to the provider).
    efforts_json TEXT NOT NULL CHECK (
        json_valid(efforts_json) AND json_type(efforts_json) = 'array'
    ),
    image_input INTEGER NOT NULL CHECK (image_input IN (0, 1)),
    tools INTEGER NOT NULL CHECK (tools IN (0, 1)),
    context_tokens INTEGER CHECK (context_tokens IS NULL OR context_tokens > 0),
    position INTEGER NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0)
) STRICT;
CREATE INDEX router_models_order ON router_models(position, id);

-- One row per tier once it handed out its first turn; `next` counts the turns taken.
CREATE TABLE router_tier_cursors (
    tier TEXT PRIMARY KEY CHECK (tier IN ('Max', 'High', 'Mid', 'Low')),
    next INTEGER NOT NULL CHECK (next >= 0)
) STRICT;

-- One pin per session. `model_id` is not a foreign key: a model removed from the catalog leaves
-- its pins ineligible (and failed over at the next request), never silently deleted.
CREATE TABLE router_pins (
    session_id TEXT PRIMARY KEY CHECK (length(session_id) BETWEEN 1 AND 128),
    pin_id TEXT NOT NULL UNIQUE CHECK (length(pin_id) BETWEEN 1 AND 128),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    model_id TEXT NOT NULL CHECK (length(model_id) BETWEEN 1 AND 128),
    connection_id TEXT NOT NULL CHECK (length(connection_id) BETWEEN 1 AND 128),
    intent_json TEXT NOT NULL CHECK (json_valid(intent_json) AND json_type(intent_json) = 'object'),
    requested_effort TEXT NOT NULL CHECK (
        requested_effort IN ('Default', 'Minimal', 'Low', 'Medium', 'High', 'XHigh', 'Max')
    ),
    effective_effort TEXT NOT NULL CHECK (
        effective_effort IN ('Default', 'Minimal', 'Low', 'Medium', 'High', 'XHigh', 'Max')
    ),
    capabilities_json TEXT NOT NULL CHECK (
        json_valid(capabilities_json) AND json_type(capabilities_json) = 'array'
    ),
    replaced_pin_id TEXT CHECK (replaced_pin_id IS NULL OR length(replaced_pin_id) BETWEEN 1 AND 128),
    reason TEXT NOT NULL CHECK (
        reason IN ('Initial', 'Failover', 'SelectionChanged', 'EffortChanged', 'ConnectionChanged')
    ),
    updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
    -- An explicit effort can never silently change (ROUTE-20).
    CHECK (requested_effort = 'Default' OR requested_effort = effective_effort)
) STRICT;

-- Router-reported cooldowns, one per model (the later expiry wins until explicitly cleared).
CREATE TABLE router_cooldowns (
    model_id TEXT PRIMARY KEY CHECK (length(model_id) BETWEEN 1 AND 128),
    until_at INTEGER NOT NULL CHECK (until_at >= 0),
    reason TEXT NOT NULL CHECK (
        reason IN ('RateLimited', 'Overloaded', 'Unreachable', 'AuthRejected', 'PlanRejected')
    ),
    observed_at INTEGER NOT NULL CHECK (observed_at >= 0)
) STRICT;

-- A logical inference request bound to its operation journal entry.
CREATE TABLE router_requests (
    request_id TEXT PRIMARY KEY CHECK (length(request_id) BETWEEN 1 AND 128),
    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
    purpose TEXT NOT NULL CHECK (
        purpose IN ('Conversation', 'Compaction', 'Title', 'Enrichment', 'Embedding', 'Vision')
    ),
    session_id TEXT,
    project_id TEXT,
    model_id TEXT NOT NULL CHECK (length(model_id) BETWEEN 1 AND 128),
    connection_id TEXT NOT NULL CHECK (length(connection_id) BETWEEN 1 AND 128),
    pin_id TEXT,
    created_at INTEGER NOT NULL CHECK (created_at >= 0)
) STRICT;
CREATE INDEX router_requests_session ON router_requests(session_id) WHERE session_id IS NOT NULL;

INSERT INTO router_models
    (id, display_name, endpoint_profile, upstream_model_id, enabled, efforts_json, image_input,
     tools, context_tokens, position, revision, updated_at)
VALUES
    ('openrouter-auto', 'Open Router Auto', 'OpenRouterChat', 'openrouter/auto', 1,
     '[]', 0, 1, NULL, 10, 1, 0),
    ('fable', 'Fable', 'ClaudeSubscriptionMessages', 'claude-fable-5-1', 1,
     '["Low","Medium","High","Max"]', 1, 1, NULL, 20, 1, 0),
    ('opus', 'Opus', 'ClaudeSubscriptionMessages', 'claude-opus-5-5', 1,
     '["Low","Medium","High","Max"]', 1, 1, NULL, 30, 1, 0),
    ('sonnet', 'Sonnet', 'ClaudeSubscriptionMessages', 'claude-sonnet-5-5', 1,
     '["Low","Medium","High","Max"]', 1, 1, NULL, 40, 1, 0),
    ('haiku', 'Haiku', 'ClaudeSubscriptionMessages', 'claude-haiku-5-5', 1,
     '["Low","Medium","High","Max"]', 1, 1, NULL, 50, 1, 0),
    ('astra', 'Astra', 'CodexSubscriptionResponses', 'gpt-6-astra', 1,
     '["Minimal","Low","Medium","High","XHigh","Max"]', 1, 1, NULL, 60, 1, 0),
    ('sol', 'Sol', 'CodexSubscriptionResponses', 'gpt-6.1-sol', 1,
     '["Low","Medium","High","XHigh"]', 1, 1, NULL, 70, 1, 0),
    ('terra', 'Terra', 'CodexSubscriptionResponses', 'gpt-5.6-terra', 1,
     '["Low","Medium","High","XHigh"]', 1, 1, NULL, 80, 1, 0),
    ('luna', 'Luna', 'CodexSubscriptionResponses', 'gpt-6-luna', 1,
     '["Low","Medium","High","XHigh"]', 1, 1, NULL, 90, 1, 0);
