# DW-131: scheduled Help cancellation owns its source repair

PR43 Ubuntu engine job128164615532 failed at the unchanged30-second wait for a
fresh file publication. The fixture removed a real generated guide and advanced
the package registry before arming its scheduled job. The real one-second Help
maintenance owner could consume that registry change and repair the guide first.
The scheduled job then legitimately reused existing bodies and never entered the
fresh-publication gate.

The original focused case passed8.09s; all34 Jobs cases with failure-only metadata
diagnostics passed139.03s. A controlled ordering experiment let production registry
maintenance complete before the scheduled job. It reproduced the exact30-second
publication wait failure (41.69s total); the scheduled operation/delivery completed,
with no HelpTopic staging remaining for the gate. This was diagnostic qualification,
not a retained timing delay or a change to the watchdog.

The final fixture keeps the real missing guide and exact scheduled child/publication
ownership checks, and removes only the unrelated registry advance. All accepted-child,
queue cancellation, unrelated delivery isolation, prior catalog/reference preservation
and unknown-outcome assertions remain, including the20-second gate and30-second wait.
Bounded metadata diagnostics print only during failure cleanup, never guide bodies.
The corrected focused case passes8.14s and all34 native Jobs cases pass129.65s.
No learned inference, OS credentials, assertion removal or timeout increase is involved.
Strict checks, fresh hosted CI/review, merge and tracker acceptance remain required.
