# Storage, quotas, logs, and import settings

Artifact location and explicit-deletion retention are harness-enforced. Per-project quota defaults to 0 (unlimited); reaching a configured limit refuses new saves rather than deleting existing originals. The single-file read limit is separate. Logs and ended overlay records have their own bounded retention.

## Back up, restore, or verify the workspace

Open **Workspace backup and restore** to create a verified manual backup or review a backup before restoring it. Select **Verify current migration ledger** to check the active store against the versions and checksums shipped with DevWorks. See [Data roots, retention, backup, and restore](help:storage-lifecycle) for the restore and reactivation steps.

## Review operational retention

Open **Review operational retention** in Storage & import. Select a record type, minimum age, and batch size, then preview. Review the cutoff, exact record IDs and revisions, and the reasons held records are kept. Confirm before the preview expires. You can cancel a queued removal and reopen the panel to see its outcome, including after restarting DevWorks.

Operational retention protects authored and referenced records. Each confirmation acts on the reviewed batch. If a candidate changes or becomes protected, request a new preview. Documents, artifacts, and published search views are outside these removal actions.

## Apply a change

Choose the intended scope, read the effective value/source, edit, validate, and save or propose through the app. Keep the original draft and operation identity if the result is uncertain. For a revision conflict, refresh and explicitly review/rebase before retrying. Adoption timing and harness-enforced restrictions are listed in the [Complete settings reference](help:settings-reference).

See [Settings overview](help:settings), [Permissions](help:permissions), and [Recovery](help:recovery).
