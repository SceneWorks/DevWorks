# GitHub account and synchronization settings

Keep the workspace GitHub.com credential in the native secret facility (`github.credential`); granted token scopes and authorization errors are inspectable without exposing the token, and local Git transport credentials remain independent of the GitHub API credential. DevWorks synchronizes personal and organization repositories through conditional paginated polling without opening a public webhook endpoint.

## Synchronization, triage, and authority

- **Change polling (`github.pollForChanges`)**: Observes default-branch ref updates, pull requests, exact-head checks, and reviews. Default-branch advances queue incremental baseline indexing independently of agent notifications.
- **Open issue intake (`github.importOpenIssues` and `github.issueTriageIntervalSeconds`)**: Imports open issues on connect and newly reported issues thereafter through an app-wide persisted admission clock (default `1800` seconds, without catch-up bursts after restarts). Shared-repository issues route across candidate projects. Bug reports are verified before automatic work starts only when a trusted check in `.devworks/github-checks.json` from the indexed commit reproduces the failure inside a network-disabled container; feature requests and unverifiable reports raise an in-app notification for human decision.
- **Linked updates and verified resolution (`github.syncLinkedIssueFields`)**: Appends remote clarifications as evidence without silently overwriting local plans, stages an approval when remote edits expand or conflict with local scope, preserves human pauses across remote reopenings, retires unstarted unreproducible bugs when closed remotely while keeping started fixes and feature requests under local control, and posts a resolution summary before closing a remote issue only once every active obligation is verified with its merged pull request observed.

## Apply a change

Choose the intended scope, read the effective value/source, edit, validate, and save or propose through the app. Keep the original draft and operation identity if the result is uncertain. For a revision conflict, refresh and explicitly review/rebase before retrying. Adoption timing and harness-enforced restrictions are listed in the [Complete settings reference](help:settings-reference).

See [Settings overview](help:settings), [Standalone issues](help:issues), [Git and delivery](help:git-delivery), [Permissions](help:permissions), and [Recovery](help:recovery).
