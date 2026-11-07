# Jobs, schedules, and run history

Jobs define durable scheduled native operations or supported agent work. Create one with a project/workspace scope, explicit schedule, definition, and enabled state. Inspect the next occurrence, eligibility, authorization, last runs, and actual registered operation before expecting it to execute. A native job selector offers only qualified operations, not every generated schema name.

## Manage a schedule

Enable and disable are different from canceling a run. Run now creates an attributable run under current admission; it does not bypass a disabled tool, archived project, unavailable runtime, pause, resource wait, or required human consent. Deleting a definition does not erase historical effects. An agent-authored definition can need exact human authorization; approval retains the original Agent actor and records the deciding human separately.

Scheduled work obeys current ownership. Jobs associated with feature work use that work's custody and budget when accounting is available; job-only totals are not an extra copy of the same feature charge. A generic scheduled operation must not create a second owner for managed work.

## Failure and recovery

Inspect a run's operation and diagnostics. Cancellation before dispatch differs from an uncertain external outcome after dispatch. Resolve the original receipt/effect before issuing a replacement. Restoring or reopening the app does not automatically resume explicitly stopped project work.

Maintenance jobs depend on their installed handlers. An unavailable Dreaming or inference runtime is not emulated. See [Controls](help:work-controls), [Recovery](help:recovery), and [Usage](help:usage).
