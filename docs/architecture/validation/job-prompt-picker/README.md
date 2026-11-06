# DW-92 prompt-picker continuation

The job editor owns bounded workspace and project prompt pages independently of the artifacts screen. Each request uses the existing artifacts.list authority and its 200-row cursor; Load more controls preserve every page and display retryable read failures. A form generation prevents late replies from an earlier form from becoming its candidates. Only workspace and the currently selected project are offered, with duplicate artifact IDs removed. Existing job edits retain their exact pinned prompt version.

The native SQLite/runtime/desktop regression seeds 210 prompts in each scope, proves the selected prompt is absent from the owner's first 200-row page, clicks the real Load more and Saved prompt controls, and saves a disabled manual job with the exact artifact/version beyond that page. No scheduled inference runs.

All 14 desktop jobs tests pass (22.98 s), including existing edit pins, scope selection, history refresh and commands. The final continuation regression passes after duplicate suppression (12.25 s), and desktop all-target strict checks pass (3.80 s). The existing macOS compact-unwind linker warning remains nonfatal. Confidence: high for local evidence; current-head automated review, complete hosted CI, merge and tracker acceptance remain required.
