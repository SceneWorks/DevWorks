# Provider adapters and codec contributions

Provider contributions declare explicit endpoint profiles and allowed endpoint IDs. Built-in native adapters and package codecs use distinct internal/public contracts; neither can choose the caller's actor, work ownership, price classification, budget, credentials, or dispatch authority.

The retained generation profiles are public Responses, Codex subscription Responses, Anthropic API Messages, Claude subscription Messages, OpenRouter Chat and compatible Chat. Embedding is a separate input/result contract. A shared brand or OpenAI-compatible syntax does not establish tools, images, parallel calls, dimensions control, effort, structured output or cache support. Advertise only actual model/endpoint/access declarations and refuse required unknown semantics before sending.

## Encode and decode faithfully

Retain the selected profile's sampling, stop, tool choice, parallel, streaming/aggregate, cache and qualified beta behavior. Codex subscription can require a streaming wire response even for an aggregate consumer, and differs from public API parameters. API-key Messages uses its pinned compatibility profile without subscription identity/OAuth injection. OpenRouter Auto and Auto Beta have distinct exact upstream/plugin identities; neither is DevWorks first-turn Auto classification.

Decode fragmented SSE/JSON without treating a bare marker or partial tool arguments as success. Preserve exact native call IDs, ordinals, tool/result structure, signed/encrypted/redacted continuation and native stop/error states. EOF, failure, incomplete, cancellation and unknown outcomes remain distinct. Complete calls are not tool-dispatch permission. Unknown native content remains evidence and must not be replayed as supported reasoning. Unsupported images are refused, never replaced by omission text.

Report nullable raw usage, exact decimal charges, actual serving identity and cache semantics to the host; never declare a free connection. Preserve unknown totals and remote tails. The host's owned transport must close/quiesce locally and respect cancellation, deadlines, byte limits and no-redirect credential fences.

End-to-end inference is available only when the actual runtime/codec/profile is qualified. No old router server or external agent CLI is required by the native design. See [Connections](help:settings-connections), [Routing](help:routing-context), [Usage](help:usage), and [Protocol](help:sdk-protocol).
