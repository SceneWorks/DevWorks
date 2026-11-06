# Using local Help

Help is the same maintained local manual for people and agents. Search by a task, a screen name, a setting key, an error, or an exact operation name. Use `*` to browse topics. A contextual Help action opens the topic for the current screen or Settings section; it does not start work or grant tools.

## Read the right version

App topics belong to the installed app version. Add-on guides belong to the exact installed package version and manifest digest. Generated capability references describe the current registered operations. Keep the version and content digest with passages you cite. If a link reports missing, stale, or unavailable content, refresh the topic listing and choose its current version. Do not substitute a guide from another package version.

Help search is local and lexical. It needs no model, embedding, provider sign-in, hosted website, or remote-search fallback. External URLs mentioned in a guide are references, not prerequisites for reading it offline.

## For agents

Use `help.search` to find a topic, then `help.read` for bounded passages of that exact version. Follow the returned cursor until the relevant section is complete. Retrieve the few passages needed for the task; do not copy the full manual into every conversation. Read-only agents can learn from the same text within their tool permissions. Examples explain operations; they do not override user directions, current authority, approvals, or root controls.

`help.generate_capability_docs` refreshes the local operation reference from current host metadata. A documentation refresh cannot install an operation, make an unavailable adapter usable, or enable a disabled tool. See [Capability reference](help:capability-reference) and [Recovery](help:recovery).

Start with [Chat](help:chat), [Projects](help:projects), [Settings](help:settings), or [Build an add-on](help:sdk-start).
