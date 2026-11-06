# Chat fragment allocation review

PR40 findings 4495134299 and 4495145001 reproduced independently using pure native
wire decoding. No provider transport or learned model executes in these tests.

The original reasoning accumulator allocated 15,435,200 bytes for 51,456 input
bytes at 256 fragments. In-place appends reduced that to 2,771,900 bytes; 2,048
fragments allocate 22,087,868 bytes for eight times the input. Both Chat profiles,
both reasoning fields and all four mergeable detail fields preserve exact Unicode
text and metadata, within the unchanged allocation guard.

The original tool-argument path allocated 8,236,014 bytes for 62,011 input bytes
at 256 fragments. Keeping its owned buffers and assembling the final native item
only at valid completion reduces this to 3,947,455 bytes; 2,048 fragments allocate
31,332,799 bytes. Exact raw arguments, parsed JSON, native ID/name/format and the
no-tool-release-before-completion rule remain checked for both Chat profiles.

Both regressions fail on their prior paths and pass after the changes. The full
retention suite passes 6/6 in 2.25 seconds; all 26 existing decode tests pass.
The bounded tests extend the existing normal non-inference test binary, with no
new workflow, download, timeout or inference workload. Assembled-head strict checks,
hosted CI, automated re-review and merge remain required.

## Older Messages and request-expansion findings

The full thread audit recovered three earlier unaddressed findings. A scalar Messages content block followed by a delta reproduced a JSON-indexing panic. Non-object blocks are now rejected before storage; every later delta fails normally. The native decoder regression covers null, string, number, array and boolean blocks with text, thinking, signature and citation deltas on both Messages profiles.

Messages text/thinking/signature deltas now append to their existing strings. A new allocation regression reproduces the old quadratic cost at only 256 fragments (18,419,398 allocated bytes for 53,871 wire bytes). The repaired decoder preserves complete signed native reasoning and exact text/signature contents at 256 and 2,048 fragments on both profiles, within the existing linear allocation bounds. All seven retention regressions pass (1.98 s); all 53 provider unit cases pass. The initial regression discarded terminal frames and used an unsigned thinking fixture; those fixture mistakes were corrected without relaxing completion or allocation assertions.

Each image occurrence now charges its expanded base64 size against the existing total request budget before reading or retaining it, including repeated references to one authorized blob. A fixture with 512 references read all 513 materials (including context) before the old encoder refused an oversized 48 KiB request. The repaired encoder refuses within four reads across the supported generation profiles. A bounded JSON writer also stops serialization before producing an oversized request buffer. Artifact ownership, source-byte validation and profile-specific wire shapes remain intact. No inference, transport, network or OS secret-store access runs in these checks.

Strict core all-target checks pass (21.57 s). Final assembled native/strict checks and new published-head CI/review remain separate merge gates. Confidence: high for the recorded local results.

The same cumulative expansion budget also covers JSON tool arguments, exact raw-argument strings, native continuation values and embedding input artifacts. A cross-profile regression exposed direct raw-argument paths in Responses/Chat that bypass JSON materialization; those now preserve the exact original bytes while charging their serialized string size. Counting uses a bounded streaming writer, so it does not allocate a complete serialized value to measure it. Repeated native/tool references are refused within five reads across all six generation profiles; embedding batches are refused within four. All 55 provider unit cases pass (0.17 s), including the existing exact-argument and wire-shape fixtures; strict core all-target checks pass (19.13 s). These extend the same request-bound repair, with no change to artifact authority or inference execution policy.

## HTTP payment refusal preserves access

Review finding 4496430546 exposed three inclusive status ranges in the native source,
owned sink and connection ledger. HTTP 402 now remains a provider/payment failure;
only 401 and 403 enter credential rejection (with existing OAuth and model-entitlement
exceptions). OpenRouter's [error documentation](https://openrouter.ai/docs/api_reference/errors-and-debugging)
identifies insufficient credits separately from invalid credentials.

The real local HTTP adapter regression first failed because OpenRouterChat became
`ReauthenticationRequired` after a 402. After repair, OpenRouterChat, Responses and
Messages retain their exact credential/account revisions and health, do not replay the
failed request or invent cooldowns, and successfully execute a new explicitly submitted
request. All five native subscription/access cases pass in 10.36 s, including genuine
401/403 rejection and OAuth refresh. Core and engine all-target strict checks pass in
31.81 s. Fixture setup initially used an invalid profile name; that failure was corrected
before the causal regression. No learned inference, real account or OS credential store
was accessed. CI cost is the added local HTTP case plus compilation; existing bounds remain.

The complete native provider suite subsequently exposed an older expected classification for
HTTP402 in `native_http_fault_categories_and_entitlement_precedence_survive_into_recorded_result`.
Its matrix now expects Provider for402, retaining every status and entitlement assertion. All43
native provider cases pass65.54s, including the full prior category matrix and explicit payment
recovery. This is an owned behavior-expectation update for the reviewed classification change.
