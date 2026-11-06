# Completion overlay presentation fixture (DW-130)

The existing NOTIFY-05 shell test deliberately injects two synthetic presentation cards and
checks layout, position, focus, stacking, expansion, durable-ack presentation and navigation.
Its real online backend can receive a late notification hint after initial settling; the
legitimate empty authoritative inbox then replaces those injected cards between assertions.
PR41 Linux lost Card b, while Windows lost the expanded Card a.

The test retains its real online backend and uses the existing `Gate::missing_hints` wrapper
to isolate this synthetic presentation state after the initial requests settle. The gate stays
open: requests are neither blocked nor replaced with successful mocked replies. Every original
assertion and timeout remains. Production notification reconciliation is unchanged.

The focused overlay test passes (6.56s), the complete default native shell suite passes25/25
(36.79s), and the separate real notification/work/durable individual-acknowledgment case passes
(13.31s). Desktop shell strict checks pass (22.08s); hosted CI/review and
merge acceptance remain separate gates. No OS credential store or learned inference is used.
