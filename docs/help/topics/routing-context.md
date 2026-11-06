# Models, routing, and context boundaries

All application inference must use the shared router: conversations, planning, review, compaction, titles, vision, semantic work, embeddings, and package/MCP-initiated inference when those producers are installed. Configuring a connection or having a provider-shaped DTO does not establish that the corresponding runtime is qualified.

## Select identity and effort

A tier is a configured pool; a named model identifies a concrete catalog entry or supported family alias. Session routing pins the selected model and connection. An eligible replacement can become the persistent pin after failover; recovery of the old model does not silently switch it back. Explicit effort is independent of tier and cannot disappear on failover. Unknown image, tool, structured-output, embedding, effort, or cache support is not inferred from a provider brand or compatible URL.

The eight stable family aliases are astra-latest, sol-latest, terra-latest, luna-latest, fable-latest, opus-latest, sonnet-latest, and haiku-latest. Each selects the highest listed numeric version within its configured profile/connection/namespace. A concrete pin preserves its exact upstream snapshot rather than following an edited catalog row. A missing model becomes unavailable without destroying user settings; a returning model does not re-enable one the user disabled.

Provider lists refresh hourly while open, manually, and when overdue after reopening. A failed or incomplete refresh preserves the last complete inventory. Subscription and API billing never cross automatically. OpenRouter's exact upstream IDs `openrouter/auto` and `openrouter/auto-beta` retain distinct provider plugins. They are distinct from DevWorks Auto classification; a listed Auto option is not proof that first-turn classification is installed. SystemOne decision models, when available, are explicit IDs outside conversational tiers.

## Context and caching

Native tool calls/results and required opaque continuation retain exact identity and order. A change of connection/account/profile/model/epoch cannot reuse foreign opaque material. Provider caching is optional and endpoint/model/access-specific. Stable prefixes help reuse but cannot override new instructions, disabled tools, required compaction, or correctness. The compaction threshold preference is 80%; an unavailable compaction runtime is not emulated.

See [Connections](help:settings-connections), [Usage](help:usage), and [Provider adapter SDK](help:sdk-provider).
