-- Pin the concrete upstream identity separately from the editable catalog row. Older writers
-- cannot preserve these snapshots; migration registration raises the writer floor to 25.
ALTER TABLE router_pins ADD COLUMN upstream_model_id TEXT
    CHECK (upstream_model_id IS NULL OR length(upstream_model_id) BETWEEN 1 AND 4096);
ALTER TABLE router_pins ADD COLUMN endpoint_profile TEXT
    CHECK (endpoint_profile IS NULL OR endpoint_profile IN (
        'OpenAiResponses', 'CodexSubscriptionResponses', 'AnthropicMessages',
        'ClaudeSubscriptionMessages', 'OpenRouterChat', 'CompatibleChat', 'Embedding'
    ));
ALTER TABLE router_pins ADD COLUMN identity_origin TEXT NOT NULL DEFAULT 'Unknown'
    CHECK (identity_origin IN ('Unknown', 'Resolved', 'Migration25CurrentCatalog'));

-- A pre-upgrade pin never recorded its historical upstream. Capture only the catalog identity
-- effective at this upgrade and label that provenance explicitly. Missing catalog rows retain
-- unknown identity and must be replaced, never guessed, if their catalog ID returns later.
UPDATE router_pins SET
    upstream_model_id = (SELECT m.upstream_model_id FROM router_models m WHERE m.id = model_id),
    endpoint_profile = (SELECT m.endpoint_profile FROM router_models m WHERE m.id = model_id),
    identity_origin = CASE WHEN EXISTS (SELECT 1 FROM router_models m WHERE m.id = model_id)
        THEN 'Migration25CurrentCatalog' ELSE 'Unknown' END;
