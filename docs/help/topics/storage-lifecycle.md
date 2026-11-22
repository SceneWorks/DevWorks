# Data roots, retention, backup, and restore

The application data root stores the workspace database and managed artifacts. Repositories and disposable worktrees are separate. `--data-dir <path>` selects a data root when starting the native desktop binary. Do not open competing writers or move managed files while the app owns the store.

Artifact content lives under managed artifact/staging locations, with metadata and exact versions in the workspace store. A Settings edit does not relocate it. Explicit data-root relocation is a lifecycle operation, not a string preference. Original artifacts remain until explicit deletion; storage pressure refuses new saves rather than evicting old evidence.

## Backup and restore

Open **Settings → Storage & import → Workspace backup and restore**. Choose a destination outside the active data root, review the checkpoint number, and select **Create workspace backup**. The folder DevWorks creates contains the SQLite checkpoint, its managed artifact and installed package files, and the verification manifest. Keep the entire folder together. Backups run only when you request them.

Choose that folder when restoring. Review the checkpoint and file count. If DevWorks asks you to migrate an older checkpoint, select **Migrate the staged backup**. This applies the app’s verified migrations to a separate copy. Approve the restore, select **Close DevWorks to restore**, then reopen the app. DevWorks verifies the saved result in the same Settings panel and keeps the stopped local workspace as recovery evidence.

Restore advances the workspace epoch. Reconnect external accounts when DevWorks asks. Re-enable the scheduled jobs and installed packages you want to use, and select **Continue** on each eligible work item you want to resume. Unresolved external effects can hold the work they affect. Keep-working starts off.

Logs and ended index overlay records have bounded retention; their linked artifacts follow artifact retention separately. Project deletion preserves repository files and shared referenced artifacts. See [Recovery](help:recovery), [Diagnostics](help:diagnostics), and [Settings](help:settings-Storage).
