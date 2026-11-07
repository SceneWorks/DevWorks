# Skills, roles, prompts, and context hooks

Content contributions declare Skill, Role, or Prompt and reference exact local artifacts. They supply contextual material, not new authority. ContextHook contributions declare Assemble, Compact, or Decision phase and priority, with exact input/output schemas and cancellation behavior.

A context proposal carries the input digest and immutable proposal artifact. The host decides whether it still matches current instructions, context epoch, ownership and scope before adoption. Do not rewrite historical transcript entries, flatten native tool exchanges, expose hidden provider reasoning, or copy foreign opaque state into another model/profile.

Role instructions cannot grant disabled tools, remove a read-only reviewer restriction, impersonate a human, or bypass approvals/controls. An agent-drafted instruction change follows the current settings proposal/approval path and adopts at its safe boundary. The app's own role settings are in [Role instructions](help:settings-Instructions).

Cache-friendly append-only context is useful only within correctness and authority. Preserve stable instructions and tool definitions where possible, but do not retain revoked tools or stale source to improve reuse. Compaction must respect the configured threshold and protected constraints; a hook cannot postpone required compaction indefinitely.

A declared hook is executable only when its actual host runtime is registered and qualified. Report unsupported phases instead of presenting a recorded schema as a working injection path. See [Routing/context](help:routing-context), [Protocol](help:sdk-protocol), and [Permissions](help:permissions).
