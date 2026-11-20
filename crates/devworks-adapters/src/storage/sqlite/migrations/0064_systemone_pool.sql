-- Decision configuration and allocation are independent of conversational model tiers.
ALTER TABLE router_decision_models ADD COLUMN decision_policy_json TEXT
    CHECK (decision_policy_json IS NULL OR json_valid(decision_policy_json));

CREATE TABLE router_decision_pool (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_model_id TEXT CHECK (last_model_id IS NULL OR length(last_model_id) BETWEEN 1 AND 128)
) STRICT;
INSERT INTO router_decision_pool (singleton, last_model_id) VALUES (1, NULL);
