# Complete Windows CI within bounded jobs

DW-125 addresses the configured job budget, without changing test behavior.

On commit `027bb5cb34538710c34f066fe032b2ea9cd5d835`, Actions run `42225961242`, Windows job `127919421703` started at 13:30:02 UTC and was cancelled at the 45-minute limit. Qualification passed. The full suite continued passing through adapters, core, desktop and engine package tests until cancellation. Ubuntu and macOS completed successfully. The direct Actions API and actual job log established cancellation.

Windows now has two independent native jobs, each retaining the existing 45-minute timeout:

- The workspace excluding `devworks-engine`, including its doc tests and the unchanged explicit Windows filesystem/Git qualification.
- The complete `devworks-engine` tests and doc tests.

Their union covers every workspace member. The required `test (windows-latest)` check retains its name and succeeds only when both jobs succeed. It runs even after failure/cancellation, so skipped or cancelled coverage cannot silently pass. Linux/macOS commands, all Clippy lanes, qualification commands, individual assertions/timeouts, inference restrictions and workflow concurrency remain unchanged.

Local validation: `actionlint` and diff checks pass. Cargo's actual workspace metadata confirms complete package coverage. All sixteen success/failure/cancelled/skipped aggregate combinations fail closed except both-success. Actual hosted Windows completion remains the acceptance gate.
