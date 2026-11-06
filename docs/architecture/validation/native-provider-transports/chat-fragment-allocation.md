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
