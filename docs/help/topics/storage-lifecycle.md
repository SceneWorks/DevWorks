# Data roots, retention, backup, and restore

The application data root stores the workspace database and managed artifacts. Repositories and disposable worktrees are separate. `--data-dir <path>` selects a data root when starting the native desktop binary. Do not open competing writers or move managed files while the app owns the store.

Artifact content lives under managed artifact/staging locations, with metadata and exact versions in the workspace store. A Settings edit does not relocate it. Explicit data-root relocation is a lifecycle operation, not a string preference. Original artifacts remain until explicit deletion; storage pressure refuses new saves rather than evicting old evidence.

## Backup and restore

Backup policy is manual; there is no automatic backup promise. Use the native backup/restore capability only when available and follow its required ownership/quiescence procedure. A database file copied without its referenced artifact generations is not necessarily a complete backup. Keep backups outside the active data root and verify the app's result.

Restore changes the store epoch. Old credentials, cursors, grants, leases, or in-flight effects cannot simply become current. Reauthenticate when required, inspect unresolved operations and restored work, then explicitly resume only what you intend. Restoring an archived project does not restart its jobs or keep-working mode.

Logs and ended index overlay records have bounded retention; their linked artifacts follow artifact retention separately. Project deletion preserves repository files and shared referenced artifacts. See [Recovery](help:recovery), [Diagnostics](help:diagnostics), and [Settings](help:settings-Storage).
