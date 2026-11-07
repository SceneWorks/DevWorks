# Native provider transport qualification

DW-19 connects native generation and embedding to the canonical router,
accounting, scoped artifact, transcript and owned-process lifecycle services.
The retained subscription reference is simple-router commit
`a556b3531b577173195bbc3876b556196447721d`.

| Profile | Native protocol |
| --- | --- |
| `CodexSubscriptionResponses` | Subscription Responses, native SSE and continuation |
| `ClaudeSubscriptionMessages` | Opt-in subscription Messages and retained identity/beta behavior |
| `AnthropicMessages` | Separate API-key Messages |
| `OpenRouterChat` | Chat Completions, reported serving identity/cost and reasoning fragments |
| `OpenAiResponses` | API-key Responses |
| `CompatibleChat` | Configured local/custom Chat Completions |
| `Embedding` | Discovered compatible embedding endpoint and actual registered dimensions |

## Dispatch and preservation

Preparation resolves exact scoped context, images, registered schemas and tool
metadata before obtaining dispatch authority. Current connection access and
price inputs are prepared by the host; historical attempt quotes remain
immutable. Actual principal/policy, session, execution owner, dispatch,
instruction/control, package and artifact checks govern admission and send.
The four request generation values retain the originating producer's opaque
generations in replay identity. They do not assert that the current host has
loaded particular instructions or confer authority.

Native continuation reads verify the original typed account/profile binding,
trusted current-store host creation provenance, and the exact version's host
origin, digest, bytes, media and current scope. Replacing an artifact version
cannot inherit its original native provenance. Ordinary OAuth rotation preserves
the account generation; endpoint/account replacement and deletion/recreation
preserve a separate durable identity high-water mark.

Incremental codecs retain valid usage before malformed tails, exact cost and
tool-argument lexemes, images, tool IDs/order and native continuation data.
Dispatchable calls and continuations wait for actual stream closure. Semantic
completion closes the owned transport without waiting indefinitely for remote
EOF. Cancellation records the request and retains an unknown remote tail.

OAuth refresh journals the exact operation and allocated secret handle before
external work. Shutdown fences short publication transactions and retains
Unknown/Reauth custody when HTTP or an already-started secret write is unclosed.
An external secret write may finish after closure; the implementation preserves
its known handle and blocks late database/inference publication.

## Evidence

The composed native source passed:

- All 24 provider integration cases, including managed redirect/Pause/principal/
  session races, native provenance/ABA, quote dimensions, actual package schemas,
  embedding population, protocol endings and transport faults.
- Both actual Engine/Tokio destruction tests: stalled source start/close, and
  stalled OAuth HTTP/already-started secret-store writes.
- The 12 pricing tests, 18 SQLite accounting cases, six managed usage cases,
  and actual pending-budget withdrawal/approval case on the merged branch.
- Workspace build, strict all-target checks for core, adapters, engine and
  desktop, formatting, and zero-drift core generation.
- All 19 integrated price lifecycle cases after the setup-receipt observation
  barrier correction.

Executable cases live in `crates/devworks-engine/tests/providers*`,
`crates/devworks-desktop/src/host/provider_refresh_shutdown_tests.rs`,
the core provider codec/materialization tests, and the adapter transport tests.
These are controlled streams, local HTTP/TLS and native process/storage fixtures;
they execute no learned inference. Live provider execution, live cache-hit
performance and platform CI are separate evidence, and are not claimed here.
Confidence is high for the exercised native behavior.
