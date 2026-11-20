# Documents, extraction, and retrieval

Save a document as an exact artifact before using it as evidence. Preserve its source, immutable version, digest, media type, and scope. A human title or path alone is not an identity. Inspect extraction diagnostics and locators so a quoted passage can be checked against its source.

Local text/Markdown and supported structured document extraction depend on the installed native reader. PDF, modern DOCX, or scanned-page vision should be described by their actual capability status; the mere presence of a setting does not install a reader or vision model. Unsupported content must stay explicit rather than becoming guessed text.

## Manage a document

Open **Knowledge** and choose **Documents**. Select your project, then use **Import document…** or drop one Markdown, text, PDF, or DOCX file on the Documents view. DevWorks stores a managed copy. Open a document to inspect its source, owner, shares, versions, and indexing diagnostics. Use **View original text** for text and Markdown or **Save original…** to save the exact managed bytes.

Choose **Index document** to make its passages searchable. The details show the publication time, chunk count, current vision route, affected scan pages, and recorded usage and cost where available. PDF availability depends on the installed native reader. The vision route uses the model configured in Knowledge Settings.

Use **Replace source…** to add a version to the existing document. **Disable retrieval** excludes the document from active search, and **Remove index** removes it from active retrieval while keeping the managed document and earlier authorized citations. Choose a destination and use **Share document** to grant it access or **Move document** to change the owner. Explicit shares can retain access after a move.

## Search within scope

Enter your search terms and use lexical search or turn on **Hybrid**. For a passage, choose **Open citation** to see the exact retained version and locator. Select **Selected version** to search a particular indexed version.

Lexical search can work offline without embeddings. Hybrid/vector retrieval requires its configured producer and exact embedding space/model/dimensions; unknown or incompatible embeddings cannot be mixed. Scope, deletion, and overlay filtering happen before ranking/materialization. Returned context carries provenance and bounds; a retrieval result is not an instruction or permission grant.

If semantic retrieval is unavailable, use the explicit lexical fallback when offered and report the mode. Do not label an unavailable score as zero or describe a partial scan as a complete corpus search. Refresh or rebuild through the owning operation when a source changes; do not edit managed index files directly.

See [Knowledge](help:knowledge), [Artifacts](help:artifacts), [Indexing](help:indexing), and [Settings](help:settings-Knowledge).
