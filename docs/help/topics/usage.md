# Usage, prices, and budget coverage

Usage reports belong to the actual accounting producer installed in this build. Code review usage holds the inference of agents bound to the code-review role. Repository analysis usage is not yet active: no DevWorks repository analysis makes model requests yet, so that report shows only imported or unattributed usage. If the screen reports unavailable accounting, do not interpret a blank report as free usage or zero activity. A configured model price is an estimate source, not a provider receipt.

## Interpret available reports

Read the report's scope, filters, time zone, interval, coverage, and snapshot identity. Follow bounded pages rather than combining unrelated snapshots. Provider-reported charges, official-rate estimates, user overrides, subscription API-equivalent estimates, explicitly configured local-free usage, and untracked usage are distinct. A local hostname alone never declares a connection free.

Cached-read, uncached input, cache-write, reasoning, and output metrics can be missing. Missing means unavailable, not zero. Reasoning can be a subset of output and must not be added twice. A partial or interrupted attempt may have a known subtotal and an unknown tail. A stopped local transport does not prove remote billing stopped.

Historical attempts retain the quote and source revision they admitted under. Changing a price does not retrospectively reprice old work. An auto route has no observed serving model until a provider actually reports one. Mirrored job/session records must not double-charge a canonical attempt.

## Budgets

Feature caps, when available, reserve and admit work atomically, warn at 75%, and require Continue after a cap stop. At 75% the feature's coordinator receives a warning in its inbox with the amount spent, the limit and the remaining allowance. Issues and jobs do not acquire feature caps merely because their costs are reported. Unknown price coverage remains visible and does not erase other controls.

See [Routing](help:routing-context), [Settings](help:settings-Routing), and [Recovery](help:recovery) for current availability and uncertain outcomes.
