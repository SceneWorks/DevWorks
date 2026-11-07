# Provider connection setup and credential recovery

Provider connections are separate from tier membership, model metadata, and prices. Add the intended profile with its exact endpoint, give it a useful name, and use the native credential/sign-in controls. Inspect its status and last successful model discovery before using it.

## Profiles and credentials

OpenAI Responses and Anthropic Messages API profiles use API keys. Anthropic API requests use the pinned versioned API profile; they do not inherit subscription identity or OAuth beta headers. Codex subscription uses device-code sign-in; Claude subscription uses direct browser authorization-code/PKCE sign-in and is disabled until a human accepts the exact risk notice. Claude subscription access is unsupported by Anthropic for third-party apps and may be refused or stopped. It is distinct from API billing and does not require a Claude CLI.

Compatible Chat and Embedding accept configured local or remote endpoints, including keyless local services where supported. Plain HTTP is an explicit compatible/local configuration exception, not a downgrade for subscription/API profiles. SystemOne connections describe explicit decision models rather than conversational tiers; actual decision invocation requires its qualified runtime.

## Default endpoints

| Profile | Default base endpoint |
| --- | --- |
| Codex subscription Responses | `https://chatgpt.com/backend-api/codex` |
| Claude subscription Messages | `https://api.anthropic.com` |
| Anthropic API Messages | `https://api.anthropic.com` |
| OpenAI API Responses | `https://api.openai.com/v1` |
| OpenRouter Chat | `https://openrouter.ai/api/v1` |
| Compatible Chat | `http://localhost:11434/v1` |
| Compatible Embedding | `http://localhost:11434/v1` |
| SystemOne decision | `http://localhost:8000/v1` |

These are defaults, not connectivity checks. Enter the endpoint actually serving your configured upstream models. Endpoint/profile/account changes can invalidate credential or risk bindings. A local-free billing control, when offered, is an explicit human-authored choice and defaults off; hostname detection cannot declare a request free.

## Refresh and edit safely

OAuth refresh is shared per connection. A network failure is availability trouble, not automatic credential revocation. Reauthentication-required means the current credential needs attention. Secret-store locking or access denial can make credentials temporarily unavailable; inspect the native facility status. Session-only secret storage does not persist credentials across restart.

Model inventories refresh hourly/manual/on overdue reopen. A failed/malformed/partial refresh keeps the last complete inventory. Missing models remain configured but unavailable. Refreshing inventory must not overwrite a human's concurrent name/endpoint/policy edit. Retain your draft on conflict and explicitly rebase it.

OpenRouter allow/exclude patterns, cost tier and provider-routing constraints remain attached to that connection and are sent on every qualified request. Never replace a subscription credential with an API key automatically. See [Routing](help:routing-context), [Approvals](help:approvals), and [Recovery](help:recovery).
