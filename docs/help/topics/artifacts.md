# Artifacts and exact versions

Artifacts hold durable files and evidence outside repositories and disposable worktrees. Save or import through the app so the content receives an immutable version, SHA-256, byte length, media type, scope, and provenance. An attachment references that exact version; a display title does not identify its contents.

## Use and share

Inspect the artifact before attaching it to a conversation, work record, review, package proposal, or browser upload. Current scope and visibility apply to every read. Sharing or moving an artifact changes access through the owning operation; a link alone does not grant another project permission. A shared artifact remains while another project still references it.

Text reads and previews can be bounded. Follow returned cursors and verify the version/digest. A preview is rebuildable and can be collected; the original artifact remains under its retention rule. Unsupported extraction should show diagnostics, not silently replace content with guessed text.

## Retention and limits

Artifacts stay until explicit deletion. Age, feature completion, worktree cleanup, and storage pressure never delete original content to make room. The default per-project quota is unlimited (0); a configured quota refuses new saves rather than evicting existing files. The single-file limit is separate. Readers, backups, and other references can defer physical collection after deletion.

A failed or uncertain save may have a journaled staging. Confirm the original operation before repeating or removing files. See [Storage](help:storage-lifecycle), [Documents](help:documents-retrieval), and [Recovery](help:recovery).
