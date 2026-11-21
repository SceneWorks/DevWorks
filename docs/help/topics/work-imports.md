# Import selected CodeGraph work and history

Open **Settings → General → Import work history**. Choose a sealed CodeGraph export from the folder shown in the dialog. Select source project or work IDs from the export. Choose **Import all projects in this snapshot** to use the entire export. Select which history and Git references to retain.

Use **Preview import** to see the source namespace, snapshot hash, selected record counts, and unavailable evidence. Read the preview report, approve that selection, and apply the snapshot. If the source file, selection, or destination changed, preview again. **Import runs** shows the durable state and report for each run. **Stop import** stops further publication and keeps any records already committed. A fresh preview and apply resumes safely from the published records.

Open **Imported work history** from a feature or card to read its source status, decisions, dates, and evidence. **Imported project history** on Overview shows source project changes and referenced sessions, including unavailable transcript evidence. Imported cards enter Backlog. Move a card on the board when you want it to take part in the current workflow. The current index appears beside a historical code reference only when the repository, commit, and file match.

## Prepare the source export

Run the exporter from the DevWorks checkout with a read-only MariaDB client account for the CodeGraph source. Provide a stable namespace for that source database. Reuse it for all snapshots of that database.

```sh
python3 tools/work-import/export_codegraph.py \
  --defaults-file codegraph-readonly.cnf --database codegraph \
  --namespace retired-codegraph --project <source-project-uuid> ./selected-work.ndjson
```

Repeat `--project` to capture more projects, or use `--all`. Supply `--attachments-root` when you have approved access to the stored evidence files. The exporter preserves verified bytes that fit the snapshot bounds and records missing attachments and harness transcripts. Keep the output with the same access restrictions as the original work history.

For recurring updates, approve the selection with **Allow a separately created schedule to use this selection**. Create an **imports.sync** native job in [Scheduled jobs](help:jobs) with its approved configuration ID and revision. Replace the sealed file with a newly captured export at the approved location. A schedule starts from the last successful source coverage with a 30-minute overlap. Run a full import first, or make the first incremental preview with an explicit lower bound. Approving a different revision requires updating the job's arguments.
