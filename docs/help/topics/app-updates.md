# App updates and restart

The lower-left sidebar shows the installed DevWorks version and any newer eligible release. Open **Updates** there or select **App updates** in **Settings → General**. DevWorks checks after launch when its saved result is due and then hourly while the app stays open. **Check now** requests a new check and honors temporary GitHub rate limits. A connection failure leaves the last checked date visible.

DevWorks uses stable releases by default. **Include prerelease versions** also checks newer prerelease builds for your OS and CPU. Changing the channel discards the current selection. DevWorks downloads verified releases in the background. You can cancel the download and check again. Development builds and installations without a verified owner may be unable to install from this panel.

Select **Review update** to see the release, source revision, target, signed manifest and workspace migration. **Approve install and restart** applies only to that exact review. Keep the current version to dismiss the review and continue working. DevWorks suspends active work for **Continue** and keeps earlier pauses. It applies the update only after it verifies shutdown and exclusive ownership of the installation. Use **Cancel update and close** while that choice is available to keep the current version.

DevWorks can restore a compatible previous app after failed startup. It keeps the workspace data and displays recovery instructions if compatibility is uncertain. After an interrupted installation that requires recovery, launch the verified compatible installed app with `--resume-update-recovery` to open the exact preserved workspace. DevWorks checks the workspace and build before enabling it. Backups and data restores are user actions. A merged DevWorks change takes effect in the installed app only after a signed release containing the change is approved and applied.

The scoped update status tool reports the current verified stage and last recorded outcome. Approval requires direct desktop confirmation. A recorded approval tracks the install and restart through the operation journal. It does not give agents permission to install or restart.
