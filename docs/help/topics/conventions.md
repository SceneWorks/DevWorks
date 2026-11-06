# Conventions and reusable guidance

Open **Knowledge / Conventions** to manage reusable guidance. This manager is separate from Knowledge claims and documents. Choose **Workspace** for general conventions or a named **Project** for that project's conventions. Switching scope, opening versions, and refreshing do not discard an editor draft.

## Find and read

Browse by title or organization, filter applicability and lifecycle, or search active body content. Search always uses active guidance; lifecycle filters apply to browsing. Search shows its actual retrieval mode and any missing coverage. A missing embedding setup is not a successful empty semantic search. **Embedding setup** opens provider settings without changing the convention scope. Search results open their exact served version; **Read section path** reads the ranked section's captured path.

**Open a convention by slug** uses the owner's exact-then-unique lookup. Slugs are literal: an empty value and whitespace are preserved when explicitly selected. Under **Resources**, the document catalog and **Exact root document** are distinct views; root-document lookup requires the exact root slug. Attachments have their own identity, revision, lifecycle and original media description. They are not additional searchable body sections.

Read Markdown or raw source in bounded passages. **Versions** shows attributable saved versions. Provenance identifies actual authors and import sources; an earlier approval marker is historical information, not permission to make a new change. Export the portable convention or download complete Markdown, raw source or an attachment. Downloads publish only after the full transfer succeeds and source integrity checks pass.

## Create, edit and organize

**New convention** prepares a portable manifest and stores it through the artifact service before importing it. Supply the literal title, slug, parent path components, order, applicability labels and Markdown. Raw source is optional. You can also **Import convention** from a portable manifest. Imports are background operations; accepted does not mean completed.

Editing first loads the complete captured source. Large sources use editing passages while keeping the complete source buffer; saving a passage does not truncate unseen text. Raw source has explicit preserve, set and remove choices. **Save convention revision** saves content and descriptive metadata. **Apply applicability and lifecycle** separately changes applicability and Active/Retired status. Retiring guidance preserves its history; deletion is a separate explicit confirmation.

Every change uses its opening revision. A concurrent change keeps the draft and shows the refusal. Inspect the updated record before choosing to rebase. Rebasing preserves the draft's original source and edits; saving replaces the inspected content with that full draft. Resource metadata and hierarchy moves have independent revision anchors. Choose a destination scope, actual parent and order under **Move and organize**; the shared service validates the hierarchy.

## Resources and operations

Upload attachments, edit their name or lifecycle, replace bytes, download a captured version, or explicitly delete a resource. A resource draft remains independent of another upload or document draft.

**Index convention** and **Index this scope** request the shared index owner. The index state, retrieval mode and setup reason come from the service. Background imports and indexing appear under **Operations** with their actual progress and cancellation availability. You can inspect a known operation ID without opening general command history.

When a save outcome is unknown, use **Confirm outcomes** to recover the exact original request. Do not submit another import to guess whether the first one committed. Prepared transfers retain their source references and can be explicitly resumed. Storage quotas and current permissions still apply; a refusal preserves the draft and does not claim that work completed.
